use godot::classes::{AudioEffectCapture, AudioServer, INode, Time};
use godot::prelude::*;
use unienc_core::{session::SessionController, temporal::TemporalController};

use crate::pipeline::{AudioRawFrame, EncodingPipeline, GodotAudioOptions, GodotVideoOptions, VideoRawFrame};

struct ActiveSession {
    controller: SessionController,
    temporal: TemporalController,
    pipeline: EncodingPipeline,
    audio_sample_position: u64,
}

/// Godot node that manages an instant-replay recording session.
///
/// Attach to any scene, configure properties in the inspector, then call
/// `start()` to begin buffering and `export_replay(seconds)` to save a clip.
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

    session: Option<Box<ActiveSession>>,
    audio_capture: Option<Gd<AudioEffectCapture>>,
    audio_capture_effect_idx: i32,
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
            session: None,
            audio_capture: None,
            audio_capture_effect_idx: -1,
        }
    }

    fn ready(&mut self) {
        self.setup_audio_capture();
    }

    fn process(&mut self, _delta: f64) {
        self.capture_frame();
    }

    fn exit_tree(&mut self) {
        self.remove_audio_capture();
    }
}

#[godot_api]
impl InstantReplayRecorder {
    /// Emitted when export_replay() finishes successfully.
    #[signal]
    fn export_completed(path: GString);

    /// Emitted on any error (invalid state, encoding failure, …).
    #[signal]
    fn error_occurred(message: GString);

    /// Begin buffering frames. Creates a new session; errors if one is already active.
    #[func]
    fn start(&mut self) {
        if self.session.is_some() {
            self.emit_error("Recording already started");
            return;
        }

        // Determine video dimensions from viewport if not set
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

        let pipeline = match EncodingPipeline::new(
            video_opts,
            audio_opts,
            self.max_memory_usage as usize,
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
    /// Emits `export_completed` on success, `error_occurred` on failure.
    #[func]
    fn export_replay(&mut self, seconds: f64) {
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

        // Drain the encoding pipeline (blocks until all buffered frames are encoded)
        session.pipeline.stop();

        let duration = if seconds > 0.0 { Some(seconds) } else { None };
        let path = if self.output_path.is_empty() {
            "replay.mp4".to_string()
        } else {
            self.output_path.to_string()
        };

        match session.pipeline.export_to_file(duration, &path) {
            Ok(()) => {
                let _ = session.controller.complete();
                self.base_mut()
                    .emit_signal("export_completed", &[GString::from(&path).to_variant()]);
            }
            Err(e) => {
                session.controller.fail();
                self.emit_error(&format!("Export failed: {e}"));
            }
        }
    }

    fn emit_error(&mut self, msg: &str) {
        self.base_mut()
            .emit_signal("error_occurred", &[GString::from(msg).to_variant()]);
    }
}

// ── Audio capture setup / teardown ──────────────────────────────────────────

impl InstantReplayRecorder {
    fn setup_audio_capture(&mut self) {
        let mut audio_server = AudioServer::singleton();
        let effect = AudioEffectCapture::new_gd();
        let bus_idx = 0i32; // Master bus
        audio_server.add_bus_effect(bus_idx, &effect);
        let effect_idx = audio_server.get_bus_effect_count(bus_idx) - 1;
        self.audio_capture = Some(effect);
        self.audio_capture_effect_idx = effect_idx;
    }

    fn remove_audio_capture(&mut self) {
        if self.audio_capture_effect_idx >= 0 {
            let mut audio_server = AudioServer::singleton();
            audio_server.remove_bus_effect(0, self.audio_capture_effect_idx);
            self.audio_capture = None;
            self.audio_capture_effect_idx = -1;
        }
    }
}

// ── Per-frame capture ────────────────────────────────────────────────────────

impl InstantReplayRecorder {
    fn capture_frame(&mut self) {
        // Check if we're recording (check paused state without holding a long borrow)
        let is_paused = self.session.as_ref().map_or(true, |s| s.temporal.is_paused());
        if is_paused {
            return;
        }

        let total_paused = self
            .session
            .as_ref()
            .map_or(0.0, |s| s.temporal.total_paused_secs());
        let timestamp =
            Time::singleton().get_ticks_usec() as f64 / 1_000_000.0 - total_paused;

        // Video ──────────────────────────────────────────────────────────────
        let video_raw = self
            .base()
            .get_viewport()
            .and_then(|vp| vp.get_texture())
            .and_then(|tex| tex.get_image())
            .map(|image| {
                let data = image.get_data();
                let mut bgra = data.to_vec();
                // Godot gives RGBA8; encoder expects BGRA32 — swap R↔B
                for chunk in bgra.chunks_mut(4) {
                    chunk.swap(0, 2);
                }
                let w = image.get_width() as u32;
                let h = image.get_height() as u32;
                VideoRawFrame { bgra32: bgra, width: w, height: h, timestamp }
            });

        if let (Some(frame), Some(session)) = (video_raw, self.session.as_ref()) {
            session.pipeline.try_send_video(frame);
        }

        // Audio ──────────────────────────────────────────────────────────────
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
                        (v.x * i16::MAX as f32)
                            .clamp(i16::MIN as f32, i16::MAX as f32) as i16,
                    );
                    samples.push(
                        (v.y * i16::MAX as f32)
                            .clamp(i16::MIN as f32, i16::MAX as f32) as i16,
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
