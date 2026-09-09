// Optional comparison adapter. Compile against the actual shared header rather
// than mirroring Swift ABI layouts in Rust. Every output is a completed lease.
#include "frame_engine.h"
#include <CoreVideo/CoreVideo.h>
#include <QuartzCore/QuartzCore.h>
#include <dlfcn.h>
#include <libavutil/display.h>
#include <libavutil/frame.h>
#include <libavutil/mastering_display_metadata.h>
#include <limits.h>
#include <math.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define API(name) static __typeof__(&name) p_##name
API(fe_session_create);
API(fe_session_submit);
API(fe_session_poll);
API(fe_output_frame);
API(fe_output_release);
API(fe_session_generation);
API(fe_session_reset);
API(fe_session_statistics);
API(fe_session_error);
API(fe_session_destroy);
API(fe_session_close);
API(fe_session_is_idle);
static void (*record_presentation)(fe_session *, uint64_t, uint64_t, double,
                                   double);
static void (*record_drop)(fe_session *, uint64_t, uint64_t);
static size_t (*measurements_json)(fe_session *, char *, size_t);
static fe_status (*measurements_configure)(fe_session *, const char *);
static void (*record_transfers)(fe_session *, uint64_t, uint64_t, int, int,
                                int);
static void *library;
static pthread_mutex_t library_lock = PTHREAD_MUTEX_INITIALIZER;

// The dylib stays loaded for the process: destroy is nonblocking and submitted
// Swift/Metal work may outlive the last caller-side session or output handle.
void *erika_hdr_create(const char *path, const char *model, uint32_t width,
                       uint32_t height, double strength, char *error,
                       size_t capacity) {
  pthread_mutex_lock(&library_lock);
  if (!library || !p_fe_session_is_idle) {
    if (!library)
      library = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (!library) {
      snprintf(error, capacity, "dlopen frame engine: %s", dlerror());
      pthread_mutex_unlock(&library_lock);
      return NULL;
    }
#define LOAD(name)                                                             \
  do {                                                                         \
    p_##name = (__typeof__(&name))dlsym(library, #name);                       \
    if (!p_##name) {                                                           \
      snprintf(error, capacity, "missing symbol: %s", #name);                  \
      pthread_mutex_unlock(&library_lock);                                     \
      return NULL;                                                             \
    }                                                                          \
  } while (0)
    LOAD(fe_session_create);
    LOAD(fe_session_submit);
    LOAD(fe_session_poll);
    LOAD(fe_output_frame);
    LOAD(fe_output_release);
    LOAD(fe_session_generation);
    LOAD(fe_session_reset);
    LOAD(fe_session_statistics);
    LOAD(fe_session_error);
    LOAD(fe_session_destroy);
    LOAD(fe_session_close);
    LOAD(fe_session_is_idle);
#undef LOAD
    record_presentation = dlsym(library, "fe_session_record_presentation");
    record_drop = dlsym(library, "fe_session_record_drop");
    record_transfers = dlsym(library, "fe_session_record_transfers");
    measurements_json = dlsym(library, "fe_session_measurements_json");
    measurements_configure =
        dlsym(library, "fe_session_measurements_configure");
  }
  pthread_mutex_unlock(&library_lock);
  fe_config config = {.struct_size = sizeof(config),
                      .abi_version = FE_ABI_VERSION,
                      .max_in_flight = 3,
                      .memory_limit_bytes = 512ULL * 1024 * 1024,
                      .processing_width = width,
                      .processing_height = height,
                      .reference_white_nits = 203,
                      .effect_strength = strength,
                      .colour_strength = 1,
                      .maximum_luminance_ratio = 2,
                      .model_path = model,
                      .model_version = "erika-shared-hdr"};
  fe_session *session = p_fe_session_create(&config, error, capacity);
  const char *configuration = getenv("ERIKA_FRAME_ENGINE_MEASUREMENTS");
  if (session && configuration && measurements_configure) {
    if (measurements_configure(session, configuration) != FE_ACCEPTED) {
      p_fe_session_error(session, error, capacity);
      p_fe_session_destroy(session);
      return NULL;
    }
  }
  return session;
}

int erika_hdr_submit(void *session, const AVFrame *av, int32_t numerator,
                     int32_t denominator, uint64_t frame_id,
                     uint64_t generation, uint64_t source_id) {
  if (!av || !av->data[3] || av->format != AV_PIX_FMT_VIDEOTOOLBOX ||
      denominator <= 0 || numerator <= 0)
    return FE_FAILED;
  if (av->pts == AV_NOPTS_VALUE || av->duration <= 0 ||
      av->pts > INT64_MAX / numerator || av->pts < INT64_MIN / numerator ||
      av->duration > INT64_MAX / numerator)
    return FE_FAILED;
  CVPixelBufferRef pixels = (CVPixelBufferRef)av->data[3];
  fe_frame f = {.struct_size = sizeof(f),
                .abi_version = FE_ABI_VERSION,
                .source_id = source_id,
                .stream_id = 1,
                .frame_id = frame_id,
                .generation = generation,
                .pts = {av->pts * numerator, denominator},
                .duration = {av->duration * numerator, denominator},
                .pixel_buffer = (void *)pixels,
                .pixel_format = CVPixelBufferGetPixelFormatType(pixels)};
  if (av->pts == AV_NOPTS_VALUE || f.duration.value <= 0)
    return FE_FAILED;
  f.geometry = (fe_geometry){
      .width = av->width,
      .height = av->height,
      .crop_x = av->crop_left,
      .crop_y = av->crop_top,
      .crop_width = av->width - av->crop_left - av->crop_right,
      .crop_height = av->height - av->crop_top - av->crop_bottom,
      .pixel_aspect_num =
          av->sample_aspect_ratio.num > 0 ? av->sample_aspect_ratio.num : 1,
      .pixel_aspect_den =
          av->sample_aspect_ratio.den > 0 ? av->sample_aspect_ratio.den : 1};
  AVFrameSideData *rotation =
      av_frame_get_side_data(av, AV_FRAME_DATA_DISPLAYMATRIX);
  if (rotation && rotation->size >= 9 * sizeof(int32_t))
    f.geometry.rotation_degrees =
        -av_display_rotation_get((int32_t *)rotation->data);
  f.plane_count = (uint32_t)CVPixelBufferGetPlaneCount(pixels);
  if (f.plane_count > 3)
    return FE_FAILED;
  for (uint32_t i = 0; i < f.plane_count; i++)
    f.planes[i] = (fe_plane){.width = CVPixelBufferGetWidthOfPlane(pixels, i),
                             .height = CVPixelBufferGetHeightOfPlane(pixels, i),
                             .bytes_per_row =
                                 CVPixelBufferGetBytesPerRowOfPlane(pixels, i)};
  f.colour = (fe_colour){.reference_white_nits = 203, .hlg_peak_nits = 1000};
  switch (av->color_trc) {
  case AVCOL_TRC_SMPTE2084:
    f.colour.transfer = FE_PQ;
    break;
  case AVCOL_TRC_ARIB_STD_B67:
    f.colour.transfer = FE_HLG;
    break;
  case AVCOL_TRC_IEC61966_2_1:
    f.colour.transfer = FE_SRGB;
    break;
  case AVCOL_TRC_BT709:
  case AVCOL_TRC_BT2020_10:
  case AVCOL_TRC_BT2020_12:
    f.colour.transfer = FE_BT709;
    break;
  default:
    return FE_FAILED;
  }
  switch (av->color_primaries) {
  case AVCOL_PRI_BT2020:
    f.colour.primaries = FE_BT2020;
    break;
  case AVCOL_PRI_BT709:
    f.colour.primaries = FE_BT709_PRIMARIES;
    break;
  case AVCOL_PRI_SMPTE432:
    f.colour.primaries = FE_DISPLAY_P3;
    break;
  default:
    return FE_FAILED;
  }
  switch (av->colorspace) {
  case AVCOL_SPC_BT2020_NCL:
    f.colour.matrix = FE_YUV2020;
    break;
  case AVCOL_SPC_BT709:
    f.colour.matrix = FE_YUV709;
    break;
  case AVCOL_SPC_SMPTE170M:
  case AVCOL_SPC_BT470BG:
    f.colour.matrix = FE_YUV601;
    break;
  default:
    return FE_FAILED;
  }
  f.colour.range =
      av->color_range == AVCOL_RANGE_JPEG ? FE_FULL_RANGE : FE_VIDEO_RANGE;
  // Shared chroma enum: center=0,left=1,topLeft=2,top=3,bottomLeft=4,bottom=5.
  switch (av->chroma_location) {
  case AVCHROMA_LOC_CENTER:
    f.colour.chroma_location = 0;
    break;
  case AVCHROMA_LOC_TOPLEFT:
    f.colour.chroma_location = 2;
    break;
  case AVCHROMA_LOC_TOP:
    f.colour.chroma_location = 3;
    break;
  case AVCHROMA_LOC_BOTTOMLEFT:
    f.colour.chroma_location = 4;
    break;
  case AVCHROMA_LOC_BOTTOM:
    f.colour.chroma_location = 5;
    break;
  default:
    f.colour.chroma_location = 1;
  }
  AVFrameSideData *mastering =
      av_frame_get_side_data(av, AV_FRAME_DATA_MASTERING_DISPLAY_METADATA);
  if (mastering && mastering->size >= sizeof(AVMasteringDisplayMetadata)) {
    AVMasteringDisplayMetadata *m = (void *)mastering->data;
    if (m->has_primaries) {
      for (int c = 0; c < 3; c++)
        for (int xy = 0; xy < 2; xy++)
          f.colour.mastering_xy[c * 2 + xy] =
              av_q2d(m->display_primaries[c][xy]);
      f.colour.mastering_xy[6] = av_q2d(m->white_point[0]);
      f.colour.mastering_xy[7] = av_q2d(m->white_point[1]);
    }
    if (m->has_luminance) {
      f.colour.mastering_min_nits = av_q2d(m->min_luminance);
      f.colour.mastering_max_nits = av_q2d(m->max_luminance);
    }
  }
  AVFrameSideData *light =
      av_frame_get_side_data(av, AV_FRAME_DATA_CONTENT_LIGHT_LEVEL);
  if (light && light->size >= sizeof(AVContentLightMetadata)) {
    AVContentLightMetadata *m = (void *)light->data;
    f.colour.max_cll = m->MaxCLL;
    f.colour.max_fall = m->MaxFALL;
  }
  f.source_colour = f.colour;
  return p_fe_session_submit(session, &f);
}

typedef struct {
  void *pixels;
  uint64_t frame_id, generation;
  int64_t pts_value, duration_value;
  int32_t pts_scale, duration_scale;
  uint32_t width, height;
  double reference_white, crop[4], rotation, aspect;
} erika_hdr_info;
void *erika_hdr_poll(void *session, erika_hdr_info *info) {
  fe_output *output = NULL;
  if (p_fe_session_poll(session, &output) != FE_ACCEPTED)
    return NULL;
  const fe_frame *f = p_fe_output_frame(output);
  *info = (erika_hdr_info){.pixels = f->pixel_buffer,
                           .frame_id = f->frame_id,
                           .generation = f->generation,
                           .pts_value = f->pts.value,
                           .duration_value = f->duration.value,
                           .pts_scale = f->pts.timescale,
                           .duration_scale = f->duration.timescale,
                           .width = f->geometry.width,
                           .height = f->geometry.height,
                           .reference_white = f->colour.reference_white_nits,
                           .crop = {f->geometry.crop_x, f->geometry.crop_y,
                                    f->geometry.crop_width,
                                    f->geometry.crop_height},
                           .rotation = f->geometry.rotation_degrees,
                           .aspect = (double)f->geometry.pixel_aspect_num /
                                     f->geometry.pixel_aspect_den};
  return output;
}
void erika_hdr_release(void *output) { p_fe_output_release(output); }
uint64_t erika_hdr_generation(void *session) {
  return p_fe_session_generation(session);
}
uint64_t erika_hdr_reset(void *session) { return p_fe_session_reset(session); }
void erika_hdr_destroy(void *session) { p_fe_session_destroy(session); }
size_t erika_hdr_error(void *session, char *error, size_t capacity) {
  return p_fe_session_error(session, error, capacity);
}
double erika_hdr_host_time(void) { return CACurrentMediaTime(); }
void erika_hdr_presented(void *session, uint64_t gen, uint64_t frame,
                         double host, double av_offset) {
  if (record_presentation)
    record_presentation(session, gen, frame, host, av_offset);
}
void erika_hdr_dropped(void *session, uint64_t gen, uint64_t frame) {
  if (record_drop)
    record_drop(session, gen, frame);
}
void erika_hdr_report(void *session, const char *path) {
  fe_statistics stats = {0};
  p_fe_session_statistics(session, &stats);
  fprintf(stderr,
          "Erika shared HDR: submitted=%llu completed=%llu cancelled=%llu "
          "failures=%llu peak_slots=%u peak_bytes=%llu\n",
          stats.submitted, stats.completed, stats.cancelled, stats.failures,
          stats.peak_slots, stats.peak_retained_bytes);
  if (path && measurements_json) {
    size_t size = measurements_json(session, NULL, 0);
    for (int retry = 0; retry < 4; retry++) {
      char *json = malloc(size);
      if (!json)
        break;
      size_t required = measurements_json(session, json, size);
      if (required > size) {
        free(json);
        size = required;
        continue;
      }
      char staged[4096];
      snprintf(staged, sizeof(staged), "%s.part", path);
      FILE *file = fopen(staged, "wb");
      if (file) {
        size_t written = fwrite(json, 1, strlen(json), file);
        int status = fclose(file);
        if (written == strlen(json) && status == 0)
          rename(staged, path);
        else
          remove(staged);
      }
      free(json);
      break;
    }
  }
}

void erika_hdr_capture(void *session, void *lease, const char *directory) {
  const fe_frame *f = p_fe_output_frame(lease);
  CVPixelBufferRef pixels = f->pixel_buffer;
  if (CVPixelBufferGetPixelFormatType(pixels) != kCVPixelFormatType_64RGBAHalf)
    return;
  char file[4096], meta[4096];
  snprintf(file, sizeof(file), "%s/g%llu-f%llu.rgba16f", directory,
           f->generation, f->frame_id);
  snprintf(meta, sizeof(meta), "%s/g%llu-f%llu.json", directory, f->generation,
           f->frame_id);
  if (CVPixelBufferLockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly) !=
      kCVReturnSuccess)
    return;
  size_t width = CVPixelBufferGetWidth(pixels),
         height = CVPixelBufferGetHeight(pixels),
         stride = CVPixelBufferGetBytesPerRow(pixels);
  const uint8_t *bytes = CVPixelBufferGetBaseAddress(pixels);
  double minimum[3] = {INFINITY, INFINITY, INFINITY},
         maximum[3] = {-INFINITY, -INFINITY, -INFINITY};
  uint64_t above = 0, bad = 0;
  FILE *out = fopen(file, "wb");
  for (size_t y = 0; y < height; y++) {
    const __fp16 *row = (const __fp16 *)(bytes + y * stride);
    if (out)
      fwrite(row, 8, width, out);
    for (size_t x = 0; x < width; x++) {
      int bright = 0;
      for (int c = 0; c < 3; c++) {
        double v = row[4 * x + c];
        if (!isfinite(v)) {
          bad++;
          continue;
        }
        minimum[c] = fmin(minimum[c], v);
        maximum[c] = fmax(maximum[c], v);
        bright |= v > f->colour.reference_white_nits;
      }
      above += bright;
    }
  }
  if (out)
    fclose(out);
  CVPixelBufferUnlockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
  for (int c = 0; c < 3; c++) {
    if (!isfinite(minimum[c]))
      minimum[c] = 0;
    if (!isfinite(maximum[c]))
      maximum[c] = 0;
  }
  out = fopen(meta, "w");
  if (out) {
    fprintf(
        out,
        "{\"boundary\":\"completed shared-engine output before native display "
        "mapping\",\"primaries\":\"BT.2020\",\"transfer\":\"linear\",\"units\":"
        "\"cd/m2\",\"format\":\"RGBA binary16 "
        "little-endian\",\"width\":%zu,\"height\":%zu,\"ptsValue\":%lld,"
        "\"ptsTimescale\":%d,\"durationValue\":%lld,\"durationTimescale\":%d,"
        "\"generation\":%llu,\"frameID\":%llu,\"minRGB\":[%.9g,%.9g,%.9g],"
        "\"maxRGB\":[%.9g,%.9g,%.9g],\"aboveReferenceWhite\":%llu,"
        "\"nonFinite\":%llu}\n",
        width, height, f->pts.value, f->pts.timescale, f->duration.value,
        f->duration.timescale, f->generation, f->frame_id, minimum[0],
        minimum[1], minimum[2], maximum[0], maximum[1], maximum[2], above, bad);
    fclose(out);
  }
  if (record_transfers)
    record_transfers(session, f->generation, f->frame_id, 0, 1, 0);
}
void erika_hdr_configure(void *session, const char *json) {
  if (measurements_configure)
    measurements_configure(session, json);
}
void erika_hdr_seek_complete(void *session, double seconds) {
  void (*fn)(fe_session *, double) = dlsym(library, "fe_session_record_seek");
  if (fn)
    fn(session, seconds);
}

int erika_hdr_close_ready(void *session) {
  p_fe_session_close(session);
  return p_fe_session_is_idle(session);
}
