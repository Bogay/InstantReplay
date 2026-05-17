use godot::prelude::*;
use std::sync::Arc;
use unienc_core::{
    buffer::BoundedEncodedFrameBuffer,
    session::SessionController,
    temporal::TemporalController,
};

struct SessionInner {
    controller: SessionController,
    temporal: TemporalController,
    buffer: BoundedEncodedFrameBuffer,
}

/// Godot node that manages an instant-replay recording session.
///
/// Attach to any scene, configure properties in the inspector, then call
/// `start()` to begin buffering and `export_replay(seconds)` to save a clip.
#[derive(GodotClass)]
#[class(base=Node)]
pub struct InstantReplayRecorder {
    base: Base<Node>,

    /// Maximum buffered duration in seconds (informational; actual limit is memory-based).
    #[export]
    max_duration: f64,

    /// Target video bitrate in bits per second.
    #[export]
    bitrate: i64,

    /// Output file path. If empty, defaults to "replay.mp4" in the working directory.
    #[export]
    output_path: GString,

    /// Memory ceiling for the encoded frame buffer in bytes.
    #[export]
    max_memory_usage: i64,

    session: Option<Arc<SessionInner>>,
}

#[godot_api]
impl INode for InstantReplayRecorder {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            max_duration: 30.0,
            bitrate: 8_000_000,
            output_path: GString::new(),
            max_memory_usage: 256 * 1024 * 1024, // 256 MiB
            session: None,
        }
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
        let temporal = TemporalController::new();
        temporal.resume();
        self.session = Some(Arc::new(SessionInner {
            controller: SessionController::new(),
            temporal,
            buffer: BoundedEncodedFrameBuffer::new(self.max_memory_usage as usize),
        }));
    }

    /// Pause buffering (frames arriving while paused are discarded by the temporal adjuster).
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
        let Some(session) = self.session.take() else {
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
        let (video_frames, audio_frames) = session.buffer.get_frames_for_duration(duration);

        // TODO (Phase 3): feed video_frames / audio_frames into unienc muxer pipeline.
        let _ = (video_frames, audio_frames);

        if session.controller.complete().is_err() {
            self.emit_error("Internal error completing export");
            return;
        }

        let path = if self.output_path.is_empty() {
            GString::from("replay.mp4")
        } else {
            self.output_path.clone()
        };

        self.base_mut()
            .emit_signal("export_completed", &[path.to_variant()]);
    }

    fn emit_error(&mut self, msg: &str) {
        self.base_mut()
            .emit_signal("error_occurred", &[GString::from(msg).to_variant()]);
    }
}
