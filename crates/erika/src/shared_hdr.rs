//! Optional macOS comparison adapter for the shared C-compatible HDR engine.
//! Submission and polling never wait for inference. A three-slot engine bounds
//! work; completed leases remain alive through the native Metal consumer.
use crate::core::*;
use crate::renderer::metal::{MetalRenderer, MetalRendererConfig};
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[path = "shared_hdr_diagnostics.rs"]
mod presentation_diagnostics;
use presentation_diagnostics::PresentationDiagnostics;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OutputInfo {
    pub pixels: *mut c_void,
    pub frame_id: u64,
    pub generation: u64,
    pub pts_value: i64,
    pub duration_value: i64,
    pub pts_scale: i32,
    pub duration_scale: i32,
    pub width: u32,
    pub height: u32,
    pub reference_white: f64,
    pub crop: [f64; 4],
    pub rotation: f64,
    pub aspect: f64,
}
impl OutputInfo {
    pub(crate) fn presentation_geometry(&self) -> ([f32; 4], [f32; 4], u32, u32) {
        let w = self.crop[2].max(1.0) * self.aspect.max(0.0001);
        let h = self.crop[3].max(1.0);
        let r = self.rotation.to_radians();
        let (s, c) = r.sin_cos();
        let dw = c.abs() * w + s.abs() * h;
        let dh = s.abs() * w + c.abs() * h;
        (
            [
                self.crop[0] as f32 / self.width as f32,
                self.crop[1] as f32 / self.height as f32,
                self.crop[2] as f32 / self.width as f32,
                self.crop[3] as f32 / self.height as f32,
            ],
            [
                (c * dw / w) as f32,
                (s * dh / w) as f32,
                (-s * dw / h) as f32,
                (c * dh / h) as f32,
            ],
            dw.round().max(1.0) as u32,
            dh.round().max(1.0) as u32,
        )
    }
}

unsafe extern "C" {
    fn erika_hdr_create(
        path: *const c_char,
        model: *const c_char,
        width: u32,
        height: u32,
        strength: f64,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    fn erika_hdr_submit(
        session: *mut c_void,
        frame: *const c_void,
        num: i32,
        den: i32,
        id: u64,
        generation: u64,
        source: u64,
    ) -> i32;
    fn erika_hdr_poll(session: *mut c_void, info: *mut OutputInfo) -> *mut c_void;
    fn erika_hdr_release(output: *mut c_void);
    fn erika_hdr_generation(session: *mut c_void) -> u64;
    fn erika_hdr_reset(session: *mut c_void) -> u64;
    fn erika_hdr_close_ready(session: *mut c_void) -> i32;
    fn erika_hdr_destroy(session: *mut c_void);
    fn erika_hdr_error(session: *mut c_void, error: *mut c_char, capacity: usize) -> usize;
    fn erika_hdr_presented(
        session: *mut c_void,
        generation: u64,
        frame: u64,
        host: f64,
        av_offset: f64,
    );
    fn erika_hdr_host_time() -> f64;
    fn erika_hdr_dropped(session: *mut c_void, generation: u64, frame: u64);
    fn erika_hdr_capture(session: *mut c_void, output: *mut c_void, path: *const c_char);
    fn erika_hdr_configure(session: *mut c_void, json: *const c_char);
    fn erika_hdr_seek_complete(session: *mut c_void, seconds: f64);
    fn erika_hdr_report(session: *mut c_void, path: *const c_char);
}

struct SessionHandle {
    raw: *mut c_void,
    report: Option<CString>,
    generation: AtomicU64,
    seek_started: AtomicU64,
    unpresented_drawables: AtomicU64,
    diagnostics: Option<Mutex<PresentationDiagnostics>>,
}
unsafe impl Send for SessionHandle {}
unsafe impl Sync for SessionHandle {}
impl Drop for SessionHandle {
    fn drop(&mut self) {
        unsafe { erika_hdr_destroy(self.raw) }
    }
}
pub struct EngineOutput {
    lease: *mut c_void,
    pub info: OutputInfo,
    session: Arc<SessionHandle>,
    clock: AtomicU64,
    clock_rate: AtomicU64,
    presentations: AtomicU64,
}
impl EngineOutput {
    pub fn presentation_clock(&self) -> f64 {
        f64::from_bits(self.clock.load(Ordering::Relaxed))
    }
    pub fn presentation_clock_rate(&self) -> f64 {
        f64::from_bits(self.clock_rate.load(Ordering::Relaxed))
    }
    pub fn drawable_submitted(&self) {
        if let Some(diagnostics) = &self.session.diagnostics {
            diagnostics.lock().unwrap().submitted(Self::host_time());
        }
    }
    pub fn gpu_complete(&self, success: bool) {
        if let Some(diagnostics) = &self.session.diagnostics {
            diagnostics.lock().unwrap().gpu_complete(Self::host_time(), success);
        }
    }
    pub fn presented(&self, host: f64, av_offset: f64, drawable_id: u64) {
        let stale = self.info.generation != self.session.generation.load(Ordering::Relaxed);
        if let Some(diagnostics) = &self.session.diagnostics {
            diagnostics.lock().unwrap().presented(Self::host_time(), host, drawable_id,
                self.info.generation, self.info.frame_id, stale);
        }
        if stale {
            return;
        }
        // Apple reports zero for drawables that were never displayed or dropped.
        // Keep this separate from real presentations and never fabricate a clock.
        if !host.is_finite() || host <= 0.0 {
            self.session
                .unpresented_drawables
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.presentations.fetch_add(1, Ordering::Relaxed);
        unsafe {
            erika_hdr_presented(
                self.session.raw,
                self.info.generation,
                self.info.frame_id,
                host,
                av_offset,
            )
        }
        let seek = f64::from_bits(self.session.seek_started.swap(0, Ordering::Relaxed));
        if seek > 0.0 && host >= seek {
            unsafe { erika_hdr_seek_complete(self.session.raw, host - seek) }
        }
    }
    pub fn host_time() -> f64 {
        unsafe { erika_hdr_host_time() }
    }
}
// C leases are immutable after completed GPU work and reference-counted by the
// engine. Drop is permitted on Metal's completion thread.
unsafe impl Send for EngineOutput {}
unsafe impl Sync for EngineOutput {}
impl Drop for EngineOutput {
    fn drop(&mut self) {
        unsafe { erika_hdr_release(self.lease) }
    }
}

pub struct SharedHDRRenderer {
    inner: MetalRenderer,
    session: Arc<SessionHandle>,
    playback_generation: Option<u64>,
    engine_generation: u64,
    frame_id: u64,
    source_id: u64,
    current: Option<Arc<EngineOutput>>,
    ready: Option<Arc<EngineOutput>>,
    pending_token: Option<(u64, u64)>,
    async_video: bool,
    clock_rate: f64,
    clock_seconds: Option<f64>,
    capture: Option<CString>,
    captured_generation: Option<u64>,
    admission_drops: u64,
    stale_outputs: u64,
    decoded_frames: u64,
    display_size: (u32, u32),
    shutting_down: bool,
    headroom_range: Option<(f32, f32)>,
}

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match std::env::var(name) {
        Ok(v) => v
            .parse()
            .map_err(|_| PlayerError::Renderer(format!("invalid {name}: {v}"))),
        Err(_) => Ok(default),
    }
}
impl SharedHDRRenderer {
    pub fn from_environment(config: MetalRendererConfig) -> Result<Option<Self>> {
        let Ok(path) = std::env::var("ERIKA_FRAME_ENGINE") else {
            return Ok(None);
        };
        let mut config = config;
        config.output_mode = crate::renderer::metal::MetalOutputMode::apple_edr(
            config.output_mode.headroom().max(1.0),
        );
        let path =
            CString::new(path).map_err(|_| PlayerError::Renderer("invalid engine path".into()))?;
        let model = std::env::var("ERIKA_FRAME_ENGINE_MODEL")
            .ok()
            .map(CString::new)
            .transpose()
            .map_err(|_| PlayerError::Renderer("invalid model path".into()))?;
        let width = env_number("ERIKA_FRAME_ENGINE_WIDTH", 32_u32)?;
        let height = env_number("ERIKA_FRAME_ENGINE_HEIGHT", 24_u32)?;
        let strength = env_number("ERIKA_FRAME_ENGINE_STRENGTH", 1_f64)?;
        let report = std::env::var("ERIKA_FRAME_ENGINE_REPORT")
            .ok()
            .map(CString::new)
            .transpose()
            .map_err(|_| PlayerError::Renderer("invalid report path".into()))?;
        let capture = std::env::var("ERIKA_FRAME_ENGINE_CAPTURE")
            .ok()
            .map(CString::new)
            .transpose()
            .map_err(|_| PlayerError::Renderer("invalid capture path".into()))?;
        let mut error = [0_i8; 2048];
        let inner = MetalRenderer::with_config(config)?;
        let source_id = env_number("ERIKA_FRAME_ENGINE_SOURCE_ID", 1_u64)?;
        let session = unsafe {
            erika_hdr_create(
                path.as_ptr(),
                model.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                width,
                height,
                strength,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if session.is_null() {
            return Err(PlayerError::Renderer(
                unsafe { CStr::from_ptr(error.as_ptr()) }
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        let engine_generation = unsafe { erika_hdr_generation(session) };
        eprintln!(
            "Erika shared HDR adapter: {} model={} neural={}x{} strength={}",
            path.to_string_lossy(),
            model
                .as_ref()
                .map_or("bypass".into(), |s| s.to_string_lossy()),
            width,
            height,
            strength
        );
        Ok(Some(Self {
            inner,
            session: Arc::new(SessionHandle {
                raw: session,
                report,
                generation: AtomicU64::new(engine_generation),
                seek_started: AtomicU64::new(0),
                unpresented_drawables: AtomicU64::new(0),
                diagnostics: (std::env::var("ERIKA_ADAPTER_DIAGNOSTICS").as_deref() == Ok("1"))
                    .then(|| Mutex::new(PresentationDiagnostics::default())),
            }),
            playback_generation: None,
            engine_generation,
            frame_id: 0,
            source_id,
            current: None,
            ready: None,
            pending_token: None,
            async_video: model.is_some() && strength > 0.0,
            clock_rate: 1.0,
            clock_seconds: None,
            capture,
            captured_generation: None,
            admission_drops: 0,
            stale_outputs: 0,
            decoded_frames: 0,
            display_size: (0, 0),
            shutting_down: false,
            headroom_range: None,
        }))
    }
    fn configure_measurements(&self) {
        if self.decoded_frames != 0 {
            return;
        }
        if let Ok(raw) = std::env::var("ERIKA_FRAME_ENGINE_MEASUREMENTS") {
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) {
                value["displayWidth"] = self.display_size.0.into();
                value["displayHeight"] = self.display_size.1.into();
                if let Ok(json) = CString::new(value.to_string()) {
                    unsafe { erika_hdr_configure(self.session.raw, json.as_ptr()) }
                }
            }
        }
    }
    fn reset(&mut self) -> Result<()> {
        self.engine_generation = unsafe { erika_hdr_reset(self.session.raw) };
        self.session
            .generation
            .store(self.engine_generation, Ordering::Relaxed);
        if self.playback_generation.is_some() {
            self.session
                .seek_started
                .store(EngineOutput::host_time().to_bits(), Ordering::Relaxed)
        }
        self.current = None;
        self.ready = None;
        self.pending_token = None;
        self.frame_id = 0;
        self.inner.clear_current_frame()
    }
    fn poll(&mut self) -> Result<()> {
        if self.async_video && self.ready.is_some() { return Ok(()); }
        loop {
            let mut info = OutputInfo::default();
            let lease = unsafe { erika_hdr_poll(self.session.raw, &mut info) };
            if lease.is_null() {
                break;
            }
            let output = Arc::new(EngineOutput {
                lease,
                info,
                session: self.session.clone(),
                clock: AtomicU64::new(f64::NAN.to_bits()),
                clock_rate: AtomicU64::new(0.0_f64.to_bits()),
                presentations: AtomicU64::new(0),
            });
            if info.generation != self.engine_generation {
                self.stale_outputs += 1;
                continue;
            }
            if self.captured_generation != Some(info.generation) {
                if let Some(path) = &self.capture {
                    unsafe { erika_hdr_capture(self.session.raw, lease, path.as_ptr()) }
                }
                self.captured_generation = Some(info.generation);
            }
            if self.async_video {
                self.ready = Some(output);
                break;
            }
            if let Some(previous) = &self.current {
                if previous.presentations.load(Ordering::Relaxed) == 0 {
                    unsafe {
                        erika_hdr_dropped(
                            self.session.raw,
                            previous.info.generation,
                            previous.info.frame_id,
                        )
                    }
                }
            }
            self.inner
                .upload_shared_hdr(output.clone(), self.playback_generation.unwrap_or(1))?;
            self.current = Some(output);
        }
        let mut error = [0_i8; 2048];
        unsafe { erika_hdr_error(self.session.raw, error.as_mut_ptr(), error.len()) };
        if error[0] != 0 {
            return Err(PlayerError::Renderer(
                unsafe { CStr::from_ptr(error.as_ptr()) }
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        Ok(())
    }
}
impl Drop for SharedHDRRenderer {
    fn drop(&mut self) {
        unsafe {
            eprintln!(
                "Erika adapter admission: decoded={} rejected_full={} stale_completions={} display={}x{}",
                self.decoded_frames,
                self.admission_drops,
                self.stale_outputs,
                self.display_size.0,
                self.display_size.1
            );
            if let Some(path) = &self.session.report {
                let diagnostics = serde_json::json!({"decodedFrames":self.decoded_frames,"admissionDrops":self.admission_drops,"staleCompletions":self.stale_outputs,
                "displayWidth":self.display_size.0,"displayHeight":self.display_size.1,
                "unpresentedDrawableCallbacks":self.session.unpresented_drawables.load(Ordering::Relaxed),
                "presentationDiagnostics":self.session.diagnostics.as_ref().map(|v| serde_json::to_value(&*v.lock().unwrap()).unwrap()),
                "displayHeadroomRange":self.headroom_range,"renderer":format!("{:?}",self.inner.runtime_stats()),
                "clockMeasurement":"audio callback media time sampled before encode, extrapolated to CAMetalDrawable presentedTime at the sampled running playback rate or zero while held; later transport changes are not reconstructed; unavailable without audio",
                "asynchronousVideoClock":self.async_video,
                "limitations":["Shared HDR enhancement holds audio and media clock when the next output misses its current-frame interval; native/bypass retains its existing synchronous policy","Late enhanced overlays omitted until timestamp pairing matches","No physical display accuracy or M5 performance claim"]});
                let _ = std::fs::write(
                    format!("{}.adapter.json", path.to_string_lossy()),
                    serde_json::to_vec_pretty(&diagnostics).unwrap(),
                );
            }
            erika_hdr_report(
                self.session.raw,
                self.session
                    .report
                    .as_ref()
                    .map_or(std::ptr::null(), |p| p.as_ptr()),
            );
        }
    }
}
impl RendererBackend for SharedHDRRenderer {
    fn uses_async_video_clock(&self) -> bool { self.async_video }

    fn poll_async_video_output(&mut self) -> Result<Option<AsyncVideoOutput>> {
        if !self.async_video { return Ok(None); }
        self.poll()?;
        self.ready.as_ref().map(|output| {
            let info = &output.info;
            if info.pts_scale <= 0 || info.pts_value < 0 || info.duration_scale <= 0 || info.duration_value <= 0 {
                return Err(PlayerError::Renderer("shared HDR completion has invalid timeline".into()));
            }
            let token = self.pending_token.filter(|(id, _)| *id == info.frame_id)
                .map(|(_, token)| token).ok_or_else(|| PlayerError::Renderer("completion has no matching asynchronous credit".into()))?;
            Ok(AsyncVideoOutput { generation: self.playback_generation.unwrap_or(1), token,
                pts: Duration::from_secs_f64(info.pts_value as f64 / info.pts_scale as f64),
                duration: Duration::from_secs_f64(info.duration_value as f64 / info.duration_scale as f64) })
        }).transpose()
    }

    fn activate_async_video_output(&mut self, selected: AsyncVideoOutput) -> Result<()> {
        if self.poll_async_video_output()? != Some(selected) {
            return Err(PlayerError::Renderer("stale shared HDR activation".into()));
        }
        let output = self.ready.as_ref().expect("validated ready output");
        self.inner.upload_shared_hdr(output.clone(), selected.generation)?;
        self.current = self.ready.take();
        self.pending_token = None;
        Ok(())
    }

    fn set_presentation_clock_rate(&mut self, rate: f64) { self.clock_rate = rate; }
    fn set_source_identity(&mut self, source: &str) -> Result<()> {
        // Stable per-source identifier; generation still distinguishes each seek.
        self.source_id = source
            .as_bytes()
            .iter()
            .fold(14695981039346656037_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(1099511628211)
            });
        self.reset()
    }

    fn prepare_shutdown(&mut self) -> bool {
        if !self.shutting_down {
            self.shutting_down = true;
            self.current = None;
            self.ready = None;
            self.pending_token = None;
            let _ = self.inner.clear_current_frame();
        }
        unsafe { erika_hdr_close_ready(self.session.raw) != 0 }
    }

    fn attach_surface(&mut self, surface: PlatformSurface) -> Result<()> {
        self.display_size = surface.metrics().physical_size();
        self.configure_measurements();
        self.inner.attach_surface(surface)
    }
    fn detach_surface(&mut self) -> Result<()> {
        self.inner.detach_surface()
    }
    fn resize_surface(&mut self, metrics: SurfaceMetrics) -> Result<()> {
        self.display_size = metrics.physical_size();
        self.configure_measurements();
        self.inner.resize_surface(metrics)
    }
    fn render_test_frame(&mut self, t: f64) -> Result<()> {
        self.inner.render_test_frame(t)
    }
    fn begin_playback_generation(
        &mut self,
        generation: u64,
        clock_seconds: Option<f64>,
    ) -> Result<()> {
        self.clock_seconds = clock_seconds;
        if self.playback_generation != Some(generation) {
            self.reset()?;
            self.playback_generation = Some(generation)
        }
        Ok(())
    }
    fn upload_player_frame(&mut self, frame: &PlayerVideoFrame) -> Result<()> {
        if self.playback_generation != Some(frame.generation) {
            self.begin_playback_generation(frame.generation, None)?
        }
        let decoded = frame.frame.decoded_frame().ok_or_else(|| {
            PlayerError::Renderer("shared HDR requires decoded VideoToolbox frames".into())
        })?;
        if !decoded.is_videotoolbox() {
            return Err(PlayerError::Renderer("shared HDR requires VideoToolbox plane ownership; software decode is unsupported by this adapter".into()));
        }
        let pts = decoded
            .pts()
            .ok_or_else(|| PlayerError::Renderer("shared HDR input has no rational PTS".into()))?;
        self.decoded_frames += 1;
        if self.async_video && (frame.enhancement_token.is_none() || self.pending_token.is_some()) {
            return Err(PlayerError::Renderer("missing or duplicate asynchronous video credit".into()));
        }
        let id = self.frame_id;
        if !self.async_video { self.frame_id = self.frame_id.wrapping_add(1); }
        let status = unsafe {
            erika_hdr_submit(
                self.session.raw,
                decoded.as_ptr().cast(),
                pts.time_base.num,
                pts.time_base.den,
                id,
                self.engine_generation,
                self.source_id,
            )
        };
        match status {
            0=>{if self.async_video {
                self.pending_token = Some((id, frame.enhancement_token.expect("validated credit")));
                self.frame_id = self.frame_id.wrapping_add(1);
            } Ok(())},
            1=>{if !self.async_video {self.admission_drops+=1;unsafe{erika_hdr_dropped(self.session.raw,self.engine_generation,id)}};Err(PlayerError::RendererBackpressure("shared HDR engine full (three retained slots)".into()))},
            2|5 if !self.async_video=>Ok(()),
            _=>Err(PlayerError::Renderer("shared HDR rejected frame metadata/ownership; inspect source colour tags and rational duration".into()))
        }
    }
    fn render_current_frame(&mut self, mut context: RenderFrameContext<'_>) -> Result<bool> {
        if !self.async_video { self.poll()?; }
        let Some(output) = self.current.as_ref() else {
            return Ok(false);
        };
        if output.info.generation != self.engine_generation {
            return Ok(false);
        }
        let pts = output.info.pts_value as f64 / output.info.pts_scale as f64;
        // Overlay content is generated by the presenter for its clock. Omit it
        // while enhancement is late rather than attach a different timestamp.
        if (context.media_time.as_secs_f64() - pts).abs() > 0.001 {
            context.overlay = None;
            context.danmaku = None
        }
        context.media_time = Duration::from_secs_f64(pts.max(0.));
        context.generation = self.playback_generation.unwrap_or(1);
        output.clock.store(
            self.clock_seconds.unwrap_or(f64::NAN).to_bits(),
            Ordering::Relaxed,
        );
        output.clock_rate.store(self.clock_rate.to_bits(), Ordering::Relaxed);
        self.inner.render_current_frame(context)
    }
    fn clear_current_frame(&mut self) -> Result<()> {
        self.reset()
    }
    fn preserve_current_frame_for_transition(&mut self) -> Result<()> {
        self.reset()
    }
    fn preserve_current_frame_for_track_transition(&mut self) -> Result<()> {
        self.reset()
    }
    fn runtime_stats(&self) -> RendererRuntimeStats {
        self.inner.runtime_stats()
    }
    fn resource_stats(&self) -> RendererResourceStats {
        self.inner.resource_stats()
    }
    fn output_status(&self) -> crate::renderer::output::OutputRuntimeStatus {
        self.inner.output_status()
    }
    fn set_output_headroom(&mut self, h: f32, k: bool) {
        if k && h.is_finite() {
            self.headroom_range = Some(
                self.headroom_range
                    .map_or((h, h), |(min, max)| (min.min(h), max.max(h))),
            );
            self.inner.shared_headroom(h)
        }
    }
    fn capture_current_frame(
        &mut self,
        context: RenderFrameContext<'_>,
        w: u32,
        h: u32,
    ) -> Result<Option<RendererFrameCapture>> {
        self.inner.capture_current_frame(context, w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crop_and_non_square_pixels_rotate_into_display_geometry() {
        let info = OutputInfo {
            width: 100,
            height: 100,
            crop: [10., 20., 80., 40.],
            rotation: 90.,
            aspect: 2.,
            ..Default::default()
        };
        let (crop, inverse, width, height) = info.presentation_geometry();
        assert_eq!(crop, [0.1, 0.2, 0.8, 0.4]);
        assert_eq!((width, height), (40, 160));
        let corner = [
            0.5 - 0.5 * (inverse[0] + inverse[1]),
            0.5 - 0.5 * (inverse[2] + inverse[3]),
        ];
        assert!(corner[0].abs() < 1e-6 && (corner[1] - 1.).abs() < 1e-6);
    }
}
