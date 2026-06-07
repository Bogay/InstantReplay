use std::sync::{Arc, Mutex};

use godot::classes::{AudioEffectCapture, AudioServer, INode, ProjectSettings, RenderingServer, Time};
use godot::prelude::*;
use unienc_core::{session::SessionController, temporal::TemporalController};

use crate::pipeline::{
    AudioRawFrame, EncodingPipeline, GodotAudioOptions, GodotVideoOptions, PipelineOptions,
    VideoRawFrame,
};

struct ActiveSession {
    controller: SessionController,
    temporal: TemporalController,
    pipeline: EncodingPipeline,
    audio_sample_position: u64,
    /// Monotonically increasing video frame counter; used by fixed-frame-rate timestamping.
    video_frame_counter: u64,
}

/// Godot node that manages an instant-replay recording session.
///
/// Attach to any scene, configure properties in the inspector, then call
/// `start()` to begin buffering and `export_replay(seconds)` to save a clip.
///
/// ## Capture modes
/// - `use_gpu_readback = false` (default): uses `Viewport.get_texture().get_image()`.
/// - `use_gpu_readback = true`: reads texture bytes directly from `RenderingDevice`
///   (lower overhead on most platforms; still a CPU copy but skips Image conversion).
///
/// Future GPU-direct / zero-copy paths per platform:
/// - Vulkan (Linux/Windows): export `VkImage` as DMA-BUF → VAAPI/MediaFoundation import.
/// - Metal (macOS/iOS): `id<MTLTexture>` → VideoToolbox CVPixelBuffer import.
/// - Android: `AHardwareBuffer` export from `VkImage` → MediaCodec Surface input.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct InstantReplayRecorder {
    base: Base<Node>,

    /// Maximum buffered duration in seconds (informational; memory-bound by max_memory_usage).
    #[export]
    max_duration: f64,

    /// Target video bitrate in bits per second.
    #[export]
    video_bitrate: i64,

    /// Target audio bitrate in bits per second.
    #[export]
    audio_bitrate: i64,

    /// Output file path. Defaults to "replay.mp4" if empty.
    #[export]
    output_path: GString,

    /// Memory ceiling for the encoded frame buffer in bytes.
    #[export]
    max_memory_usage: i64,

    /// Capture width in pixels. 0 = auto-detect from viewport.
    #[export]
    video_width: i64,

    /// Capture height in pixels. 0 = auto-detect from viewport.
    #[export]
    video_height: i64,

    /// Frames per second hint for the encoder.
    #[export]
    fps_hint: i64,

    /// When true, read pixel data via RenderingDevice instead of Image API.
    /// Reduces per-frame overhead; both paths remain CPU-side copies.
    #[export]
    use_gpu_readback: bool,

    /// If > 0.0, frame timestamps are derived from a frame counter (frame_n / fps)
    /// rather than wall-clock time. This eliminates jitter at the cost of real-time accuracy.
    /// Set to 0.0 (default) to use wall-clock timestamps.
    #[export]
    fixed_frame_rate: f64,

    /// Capacity of the bounded video input channel (number of frames).
    /// Frames submitted via the frame_post_draw callback are dropped when the channel is full.
    #[export]
    video_input_queue_size: i64,

    /// Max queued audio duration in seconds before audio is dropped.
    /// Converted to a channel capacity at `start()` using the audio mix rate.
    #[export]
    audio_input_queue_size_seconds: f64,

    session: Option<Box<ActiveSession>>,
    audio_capture: Option<Gd<AudioEffectCapture>>,
    audio_capture_effect_idx: i32,
    /// Cached viewport RID used by the frame_post_draw callback.
    cached_viewport_rid: Rid,
    /// Result slot written by the background export thread; polled in _process.
    pending_export: Option<Arc<Mutex<Option<Result<String, String>>>>>,
}

#[godot_api]
impl INode for InstantReplayRecorder {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            max_duration: 30.0,
            video_bitrate: 8_000_000,
            audio_bitrate: 128_000,
            output_path: GString::new(),
            max_memory_usage: 256 * 1024 * 1024,
            video_width: 0,
            video_height: 0,
            fps_hint: 30,
            use_gpu_readback: false,
            fixed_frame_rate: 0.0,
            video_input_queue_size: 32,
            audio_input_queue_size_seconds: 1.0,
            session: None,
            audio_capture: None,
            audio_capture_effect_idx: -1,
            cached_viewport_rid: Rid::Invalid,
            pending_export: None,
        }
    }

    fn ready(&mut self) {
        self.setup_audio_capture();
        self.connect_frame_post_draw();
    }

    fn process(&mut self, _delta: f64) {
        // Video capture happens in _on_frame_post_draw (connected in ready()).
        // _process() handles audio and polls the background export thread.
        self.poll_pending_export();
        self.capture_audio();
    }

    fn exit_tree(&mut self) {
        self.disconnect_frame_post_draw();
        self.remove_audio_capture();
    }
}

#[godot_api]
impl InstantReplayRecorder {
    /// Emitted synchronously inside export_replay(), before the background
    /// thread starts. Use this to show a "Saving…" indicator immediately.
    #[signal]
    fn export_started(path: GString);

    /// Emitted when export_replay() finishes successfully.
    #[signal]
    fn export_completed(path: GString);

    /// Emitted on any error (invalid state, encoding failure, …).
    #[signal]
    fn error_occurred(message: GString);

    /// Called by RenderingServer.frame_post_draw signal every rendered frame.
    /// Captures the viewport texture and feeds it into the encoding pipeline.
    #[func]
    fn on_frame_post_draw(&mut self) {
        let is_paused = self.session.as_ref().map_or(true, |s| s.temporal.is_paused());
        if is_paused {
            return;
        }

        let total_paused = self
            .session
            .as_ref()
            .map_or(0.0, |s| s.temporal.total_paused_secs());
        let wall_clock = Time::singleton().get_ticks_usec() as f64 / 1_000_000.0 - total_paused;

        let frame_counter = self.session.as_ref().map_or(0, |s| s.video_frame_counter);
        let timestamp = compute_frame_timestamp(self.fixed_frame_rate, frame_counter, wall_clock);

        let frame = if self.use_gpu_readback {
            self.capture_video_gpu(timestamp)
        } else {
            self.capture_video_cpu(timestamp)
        };

        if let Some(session) = self.session.as_mut() {
            if let Some(frame) = frame {
                session.pipeline.try_send_video(frame);
            }
            session.video_frame_counter += 1;
        }
    }

    /// Begin buffering frames. Creates a new session; errors if one is already active.
    #[func]
    fn start(&mut self) {
        if self.session.is_some() {
            self.emit_error("Recording already started");
            return;
        }

        // Cache the viewport RID for use in frame_post_draw
        if let Some(vp) = self.base().get_viewport() {
            self.cached_viewport_rid = vp.get_viewport_rid();
        }

        // Determine video dimensions
        let (w, h) = if self.video_width > 0 && self.video_height > 0 {
            (self.video_width as u32, self.video_height as u32)
        } else if let Some(vp) = self.base().get_viewport() {
            let rect = vp.get_visible_rect();
            (rect.size.x as u32, rect.size.y as u32)
        } else {
            (1920, 1080)
        };

        let video_opts = GodotVideoOptions {
            width: w,
            height: h,
            fps_hint: self.fps_hint as u32,
            bitrate: self.video_bitrate as u32,
        };

        let audio_sample_rate = AudioServer::singleton().get_mix_rate() as u32;
        let audio_opts = GodotAudioOptions {
            sample_rate: audio_sample_rate,
            channels: 2,
            bitrate: self.audio_bitrate as u32,
        };

        let pipeline_opts = PipelineOptions::from_config(
            self.video_input_queue_size.max(0) as usize,
            self.audio_input_queue_size_seconds,
            audio_sample_rate,
        );

        let pipeline = match EncodingPipeline::new(
            video_opts,
            audio_opts,
            self.max_memory_usage as usize,
            pipeline_opts,
        ) {
            Ok(p) => p,
            Err(e) => {
                self.emit_error(&format!("Failed to start encoding pipeline: {e}"));
                return;
            }
        };

        let temporal = TemporalController::new();
        temporal.resume();

        self.session = Some(Box::new(ActiveSession {
            controller: SessionController::new(),
            temporal,
            pipeline,
            audio_sample_position: 0,
            video_frame_counter: 0,
        }));
    }

    /// Pause buffering.
    #[func]
    fn pause_recording(&mut self) {
        if let Some(s) = &self.session {
            s.temporal.pause();
        } else {
            self.emit_error("No active session");
        }
    }

    /// Resume buffering after a pause.
    #[func]
    fn resume_recording(&mut self) {
        if let Some(s) = &self.session {
            s.temporal.resume();
        } else {
            self.emit_error("No active session");
        }
    }

    /// Stop recording and export the last `seconds` of footage.
    /// Pass `seconds <= 0` to export everything in the buffer.
    /// Returns immediately; emits `export_completed` or `error_occurred` from
    /// the main thread once the background export thread finishes.
    #[func]
    fn export_replay(&mut self, seconds: f64) {
        if self.pending_export.is_some() {
            // Silently ignore — a race condition (e.g. two mob hits, or dying during
            // a save) must not interrupt the export already in progress.
            godot_print!("[InstantReplay] export_replay() called while export in progress — ignored");
            return;
        }

        let Some(mut session) = self.session.take() else {
            self.emit_error("No active recording session");
            return;
        };

        if session.controller.begin_stop().is_err() {
            self.emit_error("Cannot export: session not in Recording state");
            return;
        }
        session.temporal.pause();
        let _ = session.controller.begin_export();

        let duration = if seconds > 0.0 { Some(seconds) } else { None };
        let raw_path = if self.output_path.is_empty() {
            GString::from("user://replay.mp4")
        } else {
            self.output_path.clone()
        };
        // Resolve Godot virtual paths (user://, res://) to real filesystem paths.
        // FFmpeg receives a plain OS path; it doesn't understand Godot's VFS.
        let path = ProjectSettings::singleton()
            .globalize_path(&raw_path)
            .to_string();

        // Notify listeners immediately so they can show a "Saving…" indicator.
        self.base_mut()
            .emit_signal("export_started", &[GString::from(&path).to_variant()]);

        // Slot shared between this thread (reader) and the export thread (writer).
        let slot: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
        let slot_writer = Arc::clone(&slot);
        self.pending_export = Some(slot);

        std::thread::spawn(move || {
            // catch_unwind ensures write_pending_result is always called, even if
            // stop() or export_to_file() panics. Without this, the slot stays None
            // forever and export_replay refuses all subsequent exports.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                session.pipeline.stop();
                session
                    .pipeline
                    .export_to_file(duration, &path)
                    .map(|_| path)
                    .map_err(|e| e.to_string())
            }))
            .unwrap_or_else(|_| Err("export thread panicked unexpectedly".to_string()));
            write_pending_result(&slot_writer, result);
        });
    }

    fn emit_error(&mut self, msg: &str) {
        self.base_mut()
            .emit_signal("error_occurred", &[GString::from(msg).to_variant()]);
    }

    fn poll_pending_export(&mut self) {
        let result = self.pending_export.as_ref().and_then(|slot| {
            take_pending_result(slot)
        });
        if let Some(r) = result {
            self.pending_export = None;
            match r {
                Ok(path) => {
                    self.base_mut()
                        .emit_signal("export_completed", &[GString::from(&path).to_variant()]);
                }
                Err(e) => self.emit_error(&format!("Export failed: {e}")),
            }
        }
    }
}

// ── Export slot helpers ──────────────────────────────────────────────────────

/// Drain the export result slot.
///
/// Recovers from a poisoned mutex (caused by a prior export thread panic) so
/// the main thread does not take a secondary panic on the next `_process()` tick.
fn take_pending_result(
    slot: &Mutex<Option<Result<String, String>>>,
) -> Option<Result<String, String>> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Write the export result into the shared slot.
///
/// Recovers from a poisoned mutex so the background thread does not re-panic
/// if an earlier panic already poisoned the lock.
fn write_pending_result(
    slot: &Mutex<Option<Result<String, String>>>,
    result: Result<String, String>,
) {
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
}

/// When `fixed_fps > 0.0`, returns the timestamp derived purely from the frame counter so
/// that output timing is deterministic regardless of wall-clock drift or pauses.
/// When `fixed_fps <= 0.0`, falls through to the caller-supplied `wall_clock` value.
#[must_use]
pub fn compute_frame_timestamp(fixed_fps: f64, frame_counter: u64, wall_clock: f64) -> f64 {
    if fixed_fps > 0.0 {
        frame_counter as f64 / fixed_fps
    } else {
        wall_clock
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_frame_timestamp, take_pending_result, write_pending_result};
    use std::sync::{Arc, Mutex};

    #[test]
    fn fixed_fps_timestamps_use_frame_counter() {
        assert_eq!(compute_frame_timestamp(30.0, 0, 99.0), 0.0);
        let expected = 1.0 / 30.0;
        let actual = compute_frame_timestamp(30.0, 1, 99.0);
        assert!((actual - expected).abs() < 1e-12, "frame 1: expected {expected} got {actual}");
        let expected = 60.0 / 30.0;
        let actual = compute_frame_timestamp(30.0, 60, 99.0);
        assert!((actual - expected).abs() < 1e-12, "frame 60: expected {expected} got {actual}");
    }

    #[test]
    fn fixed_fps_disabled_falls_through_to_wall_clock() {
        assert_eq!(compute_frame_timestamp(0.0, 5, 3.14), 3.14);
        assert_eq!(compute_frame_timestamp(-1.0, 5, 7.0), 7.0);
    }

    /// Create an `Arc<Mutex<…>>` whose mutex is already poisoned.
    fn make_poisoned() -> Arc<Mutex<Option<Result<String, String>>>> {
        let m = Arc::new(Mutex::new(None::<Result<String, String>>));
        let m2 = Arc::clone(&m);
        // Thread panics while holding the lock → mutex is marked poisoned on drop.
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("intentional poison");
        })
        .join(); // join() returns Err(JoinError); that is expected
        m
    }

    #[test]
    fn take_pending_result_survives_poisoned_mutex() {
        let slot = make_poisoned();
        assert!(slot.is_poisoned(), "precondition: mutex must be poisoned");
        let result = take_pending_result(&slot);
        assert!(result.is_none());
    }

    #[test]
    fn write_pending_result_survives_poisoned_mutex() {
        let slot = make_poisoned();
        assert!(slot.is_poisoned(), "precondition: mutex must be poisoned");
        write_pending_result(&slot, Ok("out.mp4".to_string()));
        let stored = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        assert_eq!(stored, Some(Ok("out.mp4".to_string())));
    }

    // Verify the catch_unwind pattern: if the export work panics, the slot
    // still receives an Err result so poll_pending_export can clean up.
    #[test]
    fn export_thread_panic_writes_error_to_slot() {
        let slot: Arc<Mutex<Option<Result<String, String>>>> = Arc::new(Mutex::new(None));
        let slot2 = Arc::clone(&slot);

        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                panic!("simulated pipeline failure");
                #[allow(unreachable_code)]
                Ok::<String, String>("out.mp4".to_string())
            }))
            .unwrap_or_else(|_| Err("export thread panicked unexpectedly".to_string()));
            write_pending_result(&slot2, result);
        })
        .join()
        .expect("thread must not panic after catch_unwind wraps the work");

        let value = take_pending_result(&slot);
        assert!(value.is_some(), "slot must contain a result even when work panics");
        assert!(value.unwrap().is_err(), "result must be Err when work panics");
    }
}

// ── frame_post_draw signal wiring ────────────────────────────────────────────

impl InstantReplayRecorder {
    fn connect_frame_post_draw(&mut self) {
        let callable = self.base().callable("on_frame_post_draw");
        RenderingServer::singleton().connect("frame_post_draw", &callable);
    }

    fn disconnect_frame_post_draw(&mut self) {
        let callable = self.base().callable("on_frame_post_draw");
        RenderingServer::singleton().disconnect("frame_post_draw", &callable);
    }
}

// ── Video capture implementations ────────────────────────────────────────────

impl InstantReplayRecorder {
    /// CPU path: read from Viewport's texture as an Image (RGBA → BGRA).
    fn capture_video_cpu(&self, timestamp: f64) -> Option<VideoRawFrame> {
        use std::sync::atomic::{AtomicBool, Ordering};
        static LOGGED: AtomicBool = AtomicBool::new(false);

        self.base()
            .get_viewport()
            .and_then(|vp| vp.get_texture())
            .and_then(|tex| tex.get_image())
            .map(|mut image| {
                if !LOGGED.swap(true, Ordering::Relaxed) {
                    godot_print!(
                        "[InstantReplay] first frame: format={:?} size={}x{} bytes={} expected={}",
                        image.get_format(),
                        image.get_width(),
                        image.get_height(),
                        image.get_data().len(),
                        image.get_width() * image.get_height() * 4
                    );
                }
                // Normalize to RGBA8 regardless of the viewport's native format.
                // The mobile renderer on Vulkan/LLVMpipe may return FORMAT_L8 (1 byte/pixel),
                // which would cause FFmpeg to accumulate 4 frames per encoded frame (4× tiling).
                image.convert(godot::classes::image::Format::RGBA8);
                let data = image.get_data();
                let mut bgra = data.to_vec();
                for chunk in bgra.chunks_mut(4) {
                    chunk.swap(0, 2); // RGBA → BGRA
                }
                VideoRawFrame {
                    bgra32: bgra,
                    width: image.get_width() as u32,
                    height: image.get_height() as u32,
                    timestamp,
                }
            })
    }

    /// GPU readback path: read texture bytes via RenderingDevice, bypassing Image
    /// conversion overhead. The pixel data is still transferred to CPU memory.
    ///
    /// This path also retrieves the native GPU texture handle (VkImage/MTLTexture/…)
    /// via `RenderingServer::texture_get_native_handle()` for future zero-copy use.
    fn capture_video_gpu(&self, timestamp: f64) -> Option<VideoRawFrame> {
        if self.cached_viewport_rid == Rid::Invalid {
            return self.capture_video_cpu(timestamp); // fallback
        }

        let rs = RenderingServer::singleton();
        let tex_rid = rs.viewport_get_texture(self.cached_viewport_rid);

        // --- Native handle (Phase 4 zero-copy hook) -------------------------
        // `native_handle` is a raw VkImage (Vulkan) / D3D12Resource (Windows) /
        // MTLTexture (Apple) / AHardwareBuffer-backed image (Android) handle.
        // Passing it directly to the platform encoder avoids a GPU→CPU copy.
        //
        // Platform-specific zero-copy paths (to implement in future iterations):
        //
        // #[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "android")))]
        // → Export VkImage as DMA-BUF via VK_EXT_external_memory_dma_buf.
        //   Import into ffmpeg VAAPI encoder using hwupload + drm_prime filter.
        //
        // #[cfg(target_vendor = "apple")]
        // → Wrap MTLTexture in CVPixelBuffer (IOSurface-backed).
        //   Pass to VideoToolbox via CVPixelBufferCreateWithIOSurface.
        //
        // #[cfg(target_os = "android")]
        // → Export VkImage to AHardwareBuffer via VK_ANDROID_external_memory_android_hardware_buffer.
        //   Wrap in a Surface and feed to MediaCodec.createInputSurface().
        let _native_handle: u64 = rs.texture_get_native_handle(tex_rid);
        // --------------------------------------------------------------------

        // RenderingDevice readback: bypasses Godot's Image format conversion.
        let rd = rs.get_rendering_device();
        let Some(mut rd) = rd else {
            return self.capture_video_cpu(timestamp);
        };

        let raw = rd.texture_get_data(tex_rid, 0);
        if raw.is_empty() {
            return self.capture_video_cpu(timestamp);
        }

        // RenderingDevice returns RGBA8 — swap R↔B for BGRA
        let mut bgra = raw.to_vec();
        for chunk in bgra.chunks_mut(4) {
            chunk.swap(0, 2);
        }

        // Derive dimensions from the viewport
        let (w, h) = self
            .base()
            .get_viewport()
            .map(|vp| {
                let r = vp.get_visible_rect();
                (r.size.x as u32, r.size.y as u32)
            })
            .unwrap_or((1920, 1080));

        Some(VideoRawFrame { bgra32: bgra, width: w, height: h, timestamp })
    }
}

// ── Audio capture ────────────────────────────────────────────────────────────

impl InstantReplayRecorder {
    fn setup_audio_capture(&mut self) {
        let mut audio_server = AudioServer::singleton();
        let effect = AudioEffectCapture::new_gd();
        let bus_idx = 0i32;
        audio_server.add_bus_effect(bus_idx, &effect);
        let effect_idx = audio_server.get_bus_effect_count(bus_idx) - 1;
        self.audio_capture = Some(effect);
        self.audio_capture_effect_idx = effect_idx;
    }

    fn remove_audio_capture(&mut self) {
        if self.audio_capture_effect_idx >= 0 {
            AudioServer::singleton().remove_bus_effect(0, self.audio_capture_effect_idx);
            self.audio_capture = None;
            self.audio_capture_effect_idx = -1;
        }
    }

    fn capture_audio(&mut self) {
        let is_paused = self.session.as_ref().map_or(true, |s| s.temporal.is_paused());
        if is_paused {
            return;
        }

        let audio_raw = self.audio_capture.as_ref().and_then(|cap| {
            let available = cap.get_frames_available();
            if available <= 0 {
                return None;
            }
            let frames = cap.get_buffer(available);
            let len = frames.len() as usize;
            let mut samples = Vec::with_capacity(len * 2);
            for i in 0..frames.len() {
                if let Some(v) = frames.get(i) {
                    samples.push(
                        (v.x * i16::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32) as i16,
                    );
                    samples.push(
                        (v.y * i16::MAX as f32).clamp(i16::MIN as f32, i16::MAX as f32) as i16,
                    );
                }
            }
            Some(samples)
        });

        if let (Some(samples), Some(session)) = (audio_raw, self.session.as_mut()) {
            let ts = session.audio_sample_position;
            session.audio_sample_position += samples.len() as u64 / 2;
            session.pipeline.try_send_audio(AudioRawFrame {
                samples_i16: samples,
                timestamp_in_samples: ts,
            });
        }
    }
}
