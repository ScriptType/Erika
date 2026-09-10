use std::cell::RefCell;
use std::env;
use std::ffi::c_void;
use std::process;
use std::sync::OnceLock;
use std::time::Duration;

use erika::danmaku::{DanmakuItem, DanmakuMode, DanmakuTimeline};
use erika::overlay::OverlayTimeline;
use erika::presenter::{PresenterConfig, PresenterRuntime};
use erika::renderer::metal::{MetalOutputMode, MetalRendererConfig};
use erika::subtitle::{SubtitleCue, SubtitleTimeline};
use erika::{MediaRequest, MetalSurfaceHandle, PlatformSurface};

static MEDIA_URI: OnceLock<String> = OnceLock::new();
static SUBTITLE_PATH: OnceLock<String> = OnceLock::new();
static DANMAKU_PATH: OnceLock<String> = OnceLock::new();
static SMOKE_SECONDS: OnceLock<f64> = OnceLock::new();
static EDR_HEADROOM: OnceLock<f32> = OnceLock::new();
static ADAPTER_ACTIONS: OnceLock<Vec<AdapterAction>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq)]
struct AdapterAction {
    at: f64,
    action: String,
    seconds: Option<f64>,
}

fn parse_adapter_actions(raw: &str, smoke_seconds: Option<f64>) -> Result<Vec<AdapterAction>, String> {
    let limit = smoke_seconds.filter(|v| v.is_finite() && *v > 0.0 && *v <= 600.0)
        .ok_or("ERIKA_ADAPTER_ACTIONS requires --smoke-seconds in (0, 600]")?;
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| format!("invalid ERIKA_ADAPTER_ACTIONS JSON: {e}"))?;
    let items = value.as_array().ok_or("ERIKA_ADAPTER_ACTIONS must be an array")?;
    if items.len() > 64 { return Err("ERIKA_ADAPTER_ACTIONS accepts at most 64 actions".into()); }
    let mut actions = Vec::with_capacity(items.len());
    let mut previous = 0.0;
    for (index, item) in items.iter().enumerate() {
        let object = item.as_object().ok_or_else(|| format!("action {index} must be an object"))?;
        if object.keys().any(|key| !matches!(key.as_str(), "at" | "action" | "seconds")) {
            return Err(format!("action {index} contains an unknown field"));
        }
        let at = object.get("at").and_then(|v| v.as_f64())
            .filter(|v| v.is_finite() && *v >= previous && *v < limit)
            .ok_or_else(|| format!("action {index}: at must be finite, nonnegative, ordered, and before smoke timeout"))?;
        let action = object.get("action").and_then(|v| v.as_str())
            .filter(|v| matches!(*v, "pause" | "play" | "seek" | "audio-only" | "foreground"))
            .ok_or_else(|| format!("action {index}: unsupported action"))?;
        let seconds = if action == "seek" {
            Some(object.get("seconds").and_then(|v| v.as_f64())
                .filter(|v| v.is_finite() && *v >= 0.0 && Duration::try_from_secs_f64(*v).is_ok())
                .ok_or_else(|| format!("action {index}: seek requires representable nonnegative seconds"))?)
        } else {
            if object.contains_key("seconds") { return Err(format!("action {index}: seconds is only valid for seek")); }
            None
        };
        actions.push(AdapterAction { at, action: action.into(), seconds });
        previous = at;
    }
    Ok(actions)
}

fn adapter_log(mut value: serde_json::Value) {
    value["schemaVersion"] = 1.into();
    value["hostSeconds"] = unsafe { CACurrentMediaTime() }.into();
    println!("ERIKA_ADAPTER_TRANSPORT {value}");
}

unsafe extern "C" {
    fn erika_demo_run_app();
    fn CACurrentMediaTime() -> f64;
}

thread_local! {
    static DEMO: RefCell<DemoState> = RefCell::new(DemoState::new().expect("create demo state"));
}

struct DemoState {
    presenter: PresenterRuntime,
    load_attempted: bool,
    overlay_logged: bool,
    adapter_seek_done: bool,
    shutting_down: bool,
    adapter_next_action: usize,
    adapter_audio_only: bool,
    adapter_last_snapshot: Option<f64>,
    adapter_snapshots: u32,
    adapter_failures: u64,
    adapter_last_elapsed: f64,
}

impl DemoState {
    fn new() -> erika::Result<Self> {
        Ok(Self {
            presenter: PresenterRuntime::new(PresenterConfig {
                renderer: demo_renderer_config(),
                overlay: demo_overlay_timeline(),
                danmaku: Some(demo_danmaku_timeline()),
                render_test_pattern_when_idle: MEDIA_URI.get().is_none(),
                ..PresenterConfig::default()
            })?,
            load_attempted: false,
            overlay_logged: false,
            adapter_seek_done: false,
            shutting_down: false,
            adapter_next_action: 0,
            adapter_audio_only: false,
            adapter_last_snapshot: None,
            adapter_snapshots: 0,
            adapter_failures: 0,
            adapter_last_elapsed: 0.0,
        })
    }

    fn render(&mut self, time_seconds: f64) {
        if self.shutting_down {
            return;
        }
        self.adapter_last_elapsed = time_seconds;
        if !self.load_attempted {
            self.load_attempted = true;
            if env::var("ERIKA_ADAPTER_MUTE").as_deref() == Ok("1") {
                self.presenter.set_volume(0.0);
                eprintln!("Erika adapter: application audio muted; device clock remains active");
            }
            if let Some(uri) = MEDIA_URI.get() {
                match self.presenter.open(MediaRequest::new(uri)) {
                    Ok(()) => {
                        if let Some(path) = SUBTITLE_PATH.get() {
                            match self.presenter.add_external_subtitle(path) {
                                Ok(track) => eprintln!(
                                    "Erika demo added external subtitle track #{}: {path}",
                                    track.id
                                ),
                                Err(error) => {
                                    eprintln!("Erika demo external subtitle add failed: {error}")
                                }
                            }
                        }
                        match self.presenter.play() {
                            Ok(()) => {
                                eprintln!("Erika demo opened media through presenter runtime")
                            }
                            Err(error) => {
                                self.adapter_error("initial-play", time_seconds, &error.to_string());
                                eprintln!("Erika demo play failed: {error}");
                            }
                        }
                    }
                    Err(error) => {
                        self.adapter_error("open", time_seconds, &error.to_string());
                        eprintln!("Erika demo video load failed: {error}");
                    }
                }
            }
        }

        if !self.adapter_seek_done {
            if let Ok(at) = env::var("ERIKA_ADAPTER_SEEK_AT")
                .unwrap_or_default()
                .parse::<f64>()
            {
                if time_seconds >= at {
                    self.adapter_seek_done = true;
                    let target = env::var("ERIKA_ADAPTER_SEEK_TO")
                        .unwrap_or_else(|_| "0.25".into())
                        .parse::<f64>()
                        .unwrap_or(0.25);
                    self.seek_seconds(target);
                    eprintln!(
                        "Erika adapter scripted seek: host={time_seconds:.3} target={target:.3}"
                    );
                }
            }
        }
        self.run_adapter_actions(time_seconds);
        let result = if self.adapter_audio_only {
            self.presenter.audio_only_tick()
        } else {
            self.presenter.render_tick(time_seconds)
        };
        match result {
            Ok(stats) => {
                if !self.overlay_logged && stats.overlay_frames > 0 {
                    eprintln!("Erika demo overlay active through presenter runtime");
                    self.overlay_logged = true;
                }
            }
            Err(error) => {
                self.adapter_error("tick", time_seconds, &error.to_string());
                eprintln!("Erika demo render failed: {error}");
            }
        }
        self.adapter_snapshot(time_seconds);
    }

    fn adapter_error(&mut self, stage: &str, elapsed: f64, error: &str) {
        if ADAPTER_ACTIONS.get().is_none() { return; }
        self.adapter_failures += 1;
        adapter_log(serde_json::json!({"event":"tick-error", "stage":stage,
            "demoElapsedSeconds":elapsed, "error":error, "failures":self.adapter_failures}));
    }

    fn run_adapter_actions(&mut self, elapsed: f64) {
        let Some(actions) = ADAPTER_ACTIONS.get() else { return; };
        while let Some(action) = actions.get(self.adapter_next_action) {
            if elapsed < action.at { break; }
            let index = self.adapter_next_action;
            self.adapter_next_action += 1; // Never silently retry a failed action.
            let started = unsafe { CACurrentMediaTime() };
            let result = match action.action.as_str() {
                "pause" => self.presenter.pause(),
                "play" => self.presenter.play(),
                "seek" => self.presenter.seek(Duration::from_secs_f64(action.seconds.expect("validated seek"))),
                "audio-only" => { self.adapter_audio_only = true; Ok(()) },
                "foreground" => { self.adapter_audio_only = false; Ok(()) },
                _ => unreachable!("validated action"),
            };
            let ended = unsafe { CACurrentMediaTime() };
            if result.is_err() { self.adapter_failures += 1; }
            adapter_log(serde_json::json!({"event":"action", "index":index,
                "scheduledDemoElapsedSeconds":action.at, "demoElapsedSeconds":elapsed,
                "action":action.action, "seconds":action.seconds,
                "startHostSeconds":started, "endHostSeconds":ended,
                "success":result.is_ok(), "error":result.err().map(|e| e.to_string()),
                "resultScope":"public request result; subsequent snapshots/ticks establish actual state",
                "audioOnlyRoute":self.adapter_audio_only, "failures":self.adapter_failures}));
        }
    }

    fn adapter_snapshot(&mut self, elapsed: f64) {
        if ADAPTER_ACTIONS.get().is_none() || self.adapter_snapshots >= 6000
            || self.adapter_last_snapshot.is_some_and(|last| elapsed - last < 0.1) { return; }
        self.adapter_last_snapshot = Some(elapsed);
        self.adapter_snapshots += 1;
        let snapshot = self.presenter.player().playback_snapshot();
        let runtime = self.presenter.runtime_snapshot();
        adapter_log(serde_json::json!({"event":"snapshot", "demoElapsedSeconds":elapsed,
            "sample":self.adapter_snapshots, "mediaSeconds":snapshot.media_time().as_secs_f64(),
            "isPlaying":snapshot.is_playing(), "generation":snapshot.generation,
            "clockRunning":snapshot.clock.is_running(),
            "durationSeconds":self.presenter.duration().map(|v| v.as_secs_f64()),
            "eof":self.presenter.player().is_stopped_at_end(),
            "audio":{"readFrames":runtime.audio_output_read_frames,
                "writtenFrames":runtime.audio_output_written_frames,
                "queuedFrames":runtime.audio_output_queued_frames,
                "queuedSeconds":runtime.audio_output_queued_duration.map(|v| v.as_secs_f64()),
                "underflowFrames":runtime.audio_output_underflow_frames,
                "failures":runtime.stats.audio_failures},
            "audioOnlyRoute":self.adapter_audio_only, "actionsCompleted":self.adapter_next_action,
            "failures":self.adapter_failures}));
    }

    fn toggle_play_pause(&mut self) {
        let result = if self.presenter.is_playing() {
            self.presenter.pause()
        } else {
            self.presenter.play()
        };
        if let Err(error) = result {
            eprintln!("Erika demo play/pause failed: {error}");
        }
    }

    fn seek_seconds(&mut self, seconds: f64) {
        if !seconds.is_finite() || seconds < 0.0 {
            return;
        }
        if let Err(error) = self.presenter.seek(Duration::from_secs_f64(seconds)) {
            eprintln!("Erika demo seek failed: {error}");
        }
    }
}

fn demo_renderer_config() -> MetalRendererConfig {
    let Some(headroom) = EDR_HEADROOM.get().copied() else {
        return MetalRendererConfig::default();
    };
    MetalRendererConfig {
        output_mode: MetalOutputMode::apple_edr(headroom),
        ..MetalRendererConfig::default()
    }
}

fn demo_overlay_timeline() -> OverlayTimeline {
    let subtitles = SubtitleTimeline::new(vec![SubtitleCue {
        start: Duration::from_millis(500),
        end: Duration::from_secs(4),
        text: "Erika native overlay".to_string(),
    }]);
    OverlayTimeline::default().with_subtitles(subtitles)
}

fn demo_danmaku_timeline() -> DanmakuTimeline {
    if let Some(path) = DANMAKU_PATH.get() {
        match DanmakuTimeline::from_file(path) {
            Ok(timeline) => {
                eprintln!("Erika demo loaded danmaku file: {path}");
                return timeline;
            }
            Err(error) => eprintln!("Erika demo danmaku load failed: {error}"),
        }
    }
    let mut danmaku = DanmakuTimeline::default();
    danmaku
        .push(DanmakuItem {
            id: 1,
            pts: Duration::from_secs(1),
            text: "Rust danmaku timeline".to_string(),
            mode: DanmakuMode::Scroll,
            font_size: 32.0,
            color: erika::danmaku::DanmakuColor::WHITE,
            opacity: 1.0,
            is_self: false,
        })
        .expect("demo danmaku item is valid");
    danmaku
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_attach_layer(layer: *mut c_void, width: u32, height: u32, scale: f64) {
    let surface =
        PlatformSurface::Metal(MetalSurfaceHandle::new(layer as u64, width, height, scale));
    DEMO.with(|demo| {
        if let Err(error) = demo.borrow_mut().presenter.attach_surface(surface) {
            eprintln!("Erika demo attach failed: {error}");
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_resize_layer(width: u32, height: u32, scale: f64) {
    DEMO.with(|demo| {
        if let Err(error) = demo
            .borrow_mut()
            .presenter
            .resize_surface(width, height, scale)
        {
            eprintln!("Erika demo resize failed: {error}");
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_close_ready() -> bool {
    DEMO.with(|demo| {
        let mut demo = demo.borrow_mut();
        if !demo.shutting_down {
            demo.shutting_down = true;
            let _ = demo.presenter.pause();
        }
        let ready = demo.presenter.prepare_shutdown();
        if ready {
            if let Some(actions) = ADAPTER_ACTIONS.get() {
                adapter_log(serde_json::json!({"event":"shutdown", "ready":true,
                    "lastDemoElapsedSeconds":demo.adapter_last_elapsed,
                    "actionsCompleted":demo.adapter_next_action, "actionsScheduled":actions.len(),
                    "snapshots":demo.adapter_snapshots, "failures":demo.adapter_failures,
                    "allActionsAttempted":demo.adapter_next_action == actions.len()}));
            }
        }
        ready
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_update_headroom(headroom: f64) {
    DEMO.with(|demo| {
        demo.borrow_mut()
            .presenter
            .set_output_headroom(headroom as f32, headroom.is_finite())
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_render_frame(time_seconds: f64) {
    DEMO.with(|demo| demo.borrow_mut().render(time_seconds));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_toggle_play_pause() {
    DEMO.with(|demo| demo.borrow_mut().toggle_play_pause());
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_seek_seconds(seconds: f64) {
    DEMO.with(|demo| demo.borrow_mut().seek_seconds(seconds));
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_position_seconds() -> f64 {
    DEMO.with(|demo| demo.borrow().presenter.media_time().as_secs_f64())
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_duration_seconds() -> f64 {
    DEMO.with(|demo| {
        demo.borrow()
            .presenter
            .duration()
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(0.0)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_is_playing() -> bool {
    DEMO.with(|demo| demo.borrow().presenter.is_playing())
}

#[unsafe(no_mangle)]
pub extern "C" fn erika_demo_smoke_seconds() -> f64 {
    SMOKE_SECONDS.get().copied().unwrap_or(0.0)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let options = parse_args(&args).unwrap_or_else(|error| {
        eprintln!("{error}");
        eprintln!(
            "usage: cargo run -p macos_native_demo -- [--edr [HEADROOM]] [--smoke-seconds N] [--subtitle PATH] [--ass-subtitle PATH] [--danmaku PATH] [media-path-or-uri]"
        );
        process::exit(2);
    });
    if let Ok(raw) = env::var("ERIKA_ADAPTER_ACTIONS") {
        let actions = parse_adapter_actions(&raw, options.smoke_seconds).unwrap_or_else(|error| {
            adapter_log(serde_json::json!({"event":"schedule-error", "success":false, "error":error}));
            eprintln!("{error}");
            process::exit(2);
        });
        adapter_log(serde_json::json!({"event":"schedule", "actionCount":actions.len(),
            "actions":serde_json::from_str::<serde_json::Value>(&raw).expect("validated JSON"),
            "timeDomain":"at uses native demo elapsed; hostSeconds uses CACurrentMediaTime",
            "snapshotIntervalSeconds":0.1, "maximumSnapshots":6000,
            "legacySeekEnabled":env::var_os("ERIKA_ADAPTER_SEEK_AT").is_some()}));
        ADAPTER_ACTIONS.set(actions).expect("adapter actions set once");
    }
    if let Some(path) = options.subtitle_path {
        SUBTITLE_PATH.set(path).expect("subtitle path is set once");
    }
    if let Some(path) = options.danmaku_path {
        DANMAKU_PATH.set(path).expect("danmaku path is set once");
    }

    if let Some(headroom) = options.edr_headroom {
        EDR_HEADROOM
            .set(headroom)
            .expect("EDR headroom is set once");
        eprintln!("Erika demo EDR mode: RGBA16Float headroom {headroom:.2}x");
    }
    if let Some(seconds) = options.smoke_seconds {
        SMOKE_SECONDS
            .set(seconds)
            .expect("smoke seconds is set once");
        eprintln!("Erika demo smoke mode: exit after {seconds:.2}s");
    }
    if let Some(uri) = options.media_uri {
        MEDIA_URI.set(uri).expect("media URI is set once");
    }
    unsafe { erika_demo_run_app() };
}

#[derive(Debug, Clone, PartialEq)]
struct DemoOptions {
    media_uri: Option<String>,
    smoke_seconds: Option<f64>,
    edr_headroom: Option<f32>,
    subtitle_path: Option<String>,
    danmaku_path: Option<String>,
}

fn parse_args(args: &[String]) -> Result<DemoOptions, String> {
    let mut media_uri = None;
    let mut smoke_seconds = None;
    let mut edr_headroom = None;
    let mut subtitle_path = None;
    let mut danmaku_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--edr" => {
                let mut headroom = 4.0;
                if let Some(value) = args.get(index + 1) {
                    if !value.starts_with("--") && value.parse::<f32>().is_ok() {
                        index += 1;
                        headroom = args[index].parse::<f32>().map_err(|_| {
                            format!("invalid --edr headroom value: {}", args[index])
                        })?;
                    }
                }
                if !headroom.is_finite() || headroom < 1.0 {
                    return Err("--edr headroom must be finite and at least 1.0".to_string());
                }
                edr_headroom = Some(headroom);
            }
            "--smoke-seconds" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("--smoke-seconds requires a numeric value".to_string());
                };
                let seconds = value
                    .parse::<f64>()
                    .map_err(|_| format!("invalid --smoke-seconds value: {value}"))?;
                if !seconds.is_finite() || seconds <= 0.0 {
                    return Err("--smoke-seconds must be a positive finite number".to_string());
                }
                smoke_seconds = Some(seconds);
            }
            "--subtitle" | "--ass-subtitle" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(format!("{} requires a path", args[index - 1]));
                };
                if subtitle_path.replace(value.to_string()).is_some() {
                    return Err("subtitle path was provided more than once".to_string());
                }
            }
            "--danmaku" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("--danmaku requires a path".to_string());
                };
                if danmaku_path.replace(value.to_string()).is_some() {
                    return Err("danmaku path was provided more than once".to_string());
                }
            }
            "--" => {
                index += 1;
                if index >= args.len() {
                    break;
                }
                if media_uri.replace(args[index..].join(" ")).is_some() {
                    return Err("media path was provided more than once".to_string());
                }
                break;
            }
            value if value.starts_with("--") => {
                return Err(format!("unknown option: {value}"));
            }
            value => {
                if media_uri.replace(value.to_string()).is_some() {
                    return Err("media path was provided more than once".to_string());
                }
            }
        }
        index += 1;
    }
    Ok(DemoOptions {
        media_uri,
        smoke_seconds,
        edr_headroom,
        subtitle_path,
        danmaku_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_schedule_preserves_order_and_seek_target() {
        let actions = parse_adapter_actions(r#"[{"at":1,"action":"pause"},{"at":1,"action":"seek","seconds":3.5},{"at":2,"action":"audio-only"},{"at":3,"action":"foreground"},{"at":4,"action":"play"}]"#, Some(5.0)).unwrap();
        assert_eq!(actions.len(), 5);
        assert_eq!(actions[1].seconds, Some(3.5));
        assert_eq!(actions[3].action, "foreground");
        assert!(parse_adapter_actions("[]", Some(1.0)).unwrap().is_empty());
    }

    #[test]
    fn adapter_schedule_rejects_malformed_or_unbounded_requests() {
        for raw in ["{}", "[null]", r#"[{"at":-1,"action":"play"}]"#,
            r#"[{"at":1,"action":"seek"}]"#, r#"[{"at":1,"action":"seek","seconds":-1}]"#,
            r#"[{"at":1,"action":"pause","seconds":2}]"#, r#"[{"at":1,"action":"stop"}]"#,
            r#"[{"at":1,"action":"play","typo":0}]"#,
            r#"[{"at":2,"action":"pause"},{"at":1,"action":"play"}]"#,
            r#"[{"at":5,"action":"play"}]"#] {
            assert!(parse_adapter_actions(raw, Some(5.0)).is_err(), "accepted {raw}");
        }
        assert!(parse_adapter_actions("[]", None).is_err());
        assert!(parse_adapter_actions("[]", Some(601.0)).is_err());
    }

    #[test]
    fn parse_args_accepts_media_and_smoke_seconds() {
        let args = vec![
            "--smoke-seconds".to_string(),
            "1.5".to_string(),
            "/tmp/movie.mp4".to_string(),
        ];

        let options = parse_args(&args).unwrap();

        assert_eq!(options.media_uri.as_deref(), Some("/tmp/movie.mp4"));
        assert_eq!(options.smoke_seconds, Some(1.5));
        assert_eq!(options.edr_headroom, None);
        assert_eq!(options.subtitle_path, None);
        assert_eq!(options.danmaku_path, None);
    }

    #[test]
    fn parse_args_accepts_edr_with_default_headroom() {
        let args = vec!["--edr".to_string(), "/tmp/movie.mp4".to_string()];

        let options = parse_args(&args).unwrap();

        assert_eq!(options.edr_headroom, Some(4.0));
        assert_eq!(options.media_uri.as_deref(), Some("/tmp/movie.mp4"));
    }

    #[test]
    fn parse_args_accepts_edr_with_explicit_headroom() {
        let args = vec!["--edr".to_string(), "2.5".to_string()];

        let options = parse_args(&args).unwrap();

        assert_eq!(options.edr_headroom, Some(2.5));
    }

    #[test]
    fn parse_args_rejects_invalid_edr_headroom() {
        let args = vec!["--edr".to_string(), "0".to_string()];

        let error = parse_args(&args).unwrap_err();

        assert!(error.contains("headroom"));
    }

    #[test]
    fn parse_args_rejects_non_positive_smoke_seconds() {
        let args = vec!["--smoke-seconds".to_string(), "0".to_string()];

        let error = parse_args(&args).unwrap_err();

        assert!(error.contains("positive"));
    }

    #[test]
    fn parse_args_accepts_subtitle_path() {
        let args = vec![
            "--subtitle".to_string(),
            "/tmp/subs.srt".to_string(),
            "/tmp/movie.mp4".to_string(),
        ];

        let options = parse_args(&args).unwrap();

        assert_eq!(options.subtitle_path.as_deref(), Some("/tmp/subs.srt"));
        assert_eq!(options.media_uri.as_deref(), Some("/tmp/movie.mp4"));
    }

    #[test]
    fn parse_args_keeps_ass_subtitle_alias() {
        let args = vec![
            "--ass-subtitle".to_string(),
            "/tmp/subs.ass".to_string(),
            "/tmp/movie.mp4".to_string(),
        ];

        let options = parse_args(&args).unwrap();

        assert_eq!(options.subtitle_path.as_deref(), Some("/tmp/subs.ass"));
        assert_eq!(options.media_uri.as_deref(), Some("/tmp/movie.mp4"));
    }

    #[test]
    fn parse_args_accepts_danmaku_path() {
        let args = vec![
            "--danmaku".to_string(),
            "/tmp/danmaku.json".to_string(),
            "/tmp/movie.mp4".to_string(),
        ];

        let options = parse_args(&args).unwrap();

        assert_eq!(options.danmaku_path.as_deref(), Some("/tmp/danmaku.json"));
        assert_eq!(options.media_uri.as_deref(), Some("/tmp/movie.mp4"));
    }
}
