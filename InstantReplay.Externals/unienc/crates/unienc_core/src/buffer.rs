use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    Interpolated,
    Key,
    Metadata,
}

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Arc<[u8]>,
    pub timestamp: f64,
    pub kind: SampleKind,
}

impl EncodedFrame {
    pub fn new(data: impl Into<Arc<[u8]>>, timestamp: f64, kind: SampleKind) -> Self {
        Self {
            data: data.into(),
            timestamp,
            kind,
        }
    }

    pub fn with_timestamp(mut self, timestamp: f64) -> Self {
        self.timestamp = timestamp;
        self
    }
}

/// Thread-safe circular buffer for encoded frames with a hard memory ceiling.
///
/// When adding a frame would exceed `max_memory_bytes`, oldest frames are
/// dropped (video and audio interleaved by timestamp) until enough room exists.
pub struct BoundedEncodedFrameBuffer {
    inner: Mutex<BufferInner>,
    max_memory_bytes: usize,
}

struct BufferInner {
    video_queue: VecDeque<EncodedFrame>,
    audio_queue: VecDeque<EncodedFrame>,
    video_metadata: Vec<EncodedFrame>,
    audio_metadata: Vec<EncodedFrame>,
    current_memory_bytes: usize,
    video_latest_timestamp: Option<f64>,
}

impl BufferInner {
    fn new() -> Self {
        Self {
            video_queue: VecDeque::new(),
            audio_queue: VecDeque::new(),
            video_metadata: Vec::new(),
            audio_metadata: Vec::new(),
            current_memory_bytes: 0,
            video_latest_timestamp: None,
        }
    }
}

impl BoundedEncodedFrameBuffer {
    pub fn new(max_memory_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(BufferInner::new()),
            max_memory_bytes,
        }
    }

    /// Returns current memory usage in bytes.
    pub fn memory_usage(&self) -> usize {
        self.inner.lock().unwrap().current_memory_bytes
    }

    /// Adds a video frame. Returns `false` if the buffer is shutting down (currently always true).
    pub fn try_add_video_frame(&self, frame: EncodedFrame) -> bool {
        let frame_size = frame.data.len();
        let mut g = self.inner.lock().unwrap();
        self.ensure_capacity_locked(&mut g, frame_size);

        if frame.kind == SampleKind::Metadata {
            g.video_metadata.push(frame);
        } else {
            // Ignore out-of-order frames with timestamp=0 (MediaCodec quirk)
            if g.video_latest_timestamp.map_or(true, |t| frame.timestamp >= t) {
                g.video_latest_timestamp = Some(frame.timestamp);
            }
            g.video_queue.push_back(frame);
        }
        g.current_memory_bytes += frame_size;
        true
    }

    /// Adds an audio frame.
    pub fn try_add_audio_frame(&self, frame: EncodedFrame) -> bool {
        let frame_size = frame.data.len();
        let mut g = self.inner.lock().unwrap();
        self.ensure_capacity_locked(&mut g, frame_size);

        if frame.kind == SampleKind::Metadata {
            g.audio_metadata.push(frame);
        } else {
            g.audio_queue.push_back(frame);
        }
        g.current_memory_bytes += frame_size;
        true
    }

    /// Drains the buffer and returns frames trimmed to the requested duration,
    /// starting from the nearest keyframe.
    ///
    /// Timestamps are adjusted so the first video frame starts at 0.
    /// Metadata frames are prepended.
    pub fn get_frames_for_duration(
        &self,
        duration_secs: Option<f64>,
    ) -> (Vec<EncodedFrame>, Vec<EncodedFrame>) {
        let mut g = self.inner.lock().unwrap();

        let video_latest = g.video_latest_timestamp;
        let raw_video: Vec<EncodedFrame> = g.video_queue.drain(..).collect();
        let raw_audio: Vec<EncodedFrame> = g.audio_queue.drain(..).collect();
        let video_meta: Vec<EncodedFrame> = g.video_metadata.drain(..).collect();
        let audio_meta: Vec<EncodedFrame> = g.audio_metadata.drain(..).collect();

        // recalculate memory after drain
        let drained_bytes: usize = raw_video.iter().chain(raw_audio.iter()).map(|f| f.data.len()).sum::<usize>()
            + video_meta.iter().chain(audio_meta.iter()).map(|f| f.data.len()).sum::<usize>();
        g.current_memory_bytes = g.current_memory_bytes.saturating_sub(drained_bytes);
        g.video_latest_timestamp = None;
        drop(g);

        if raw_video.is_empty() {
            return (Vec::new(), Vec::new());
        }

        let latest = video_latest.unwrap_or(0.0);

        // Find the keyframe closest to the target start time
        let video_start_idx = match duration_secs {
            Some(dur) => {
                let expected_start = latest - dur;
                raw_video
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.kind == SampleKind::Key)
                    .min_by(|(_, a), (_, b)| {
                        let da = (a.timestamp - expected_start).abs();
                        let db = (b.timestamp - expected_start).abs();
                        da.partial_cmp(&db).unwrap()
                    })
                    .map(|(i, _)| i)
            }
            None => raw_video
                .iter()
                .position(|f| f.kind == SampleKind::Key),
        };

        let Some(video_start_idx) = video_start_idx else {
            return (Vec::new(), Vec::new());
        };

        // Discard frames before the keyframe (they are dropped, not returned)
        let video_frames = &raw_video[video_start_idx..];

        // Find audio start aligned to video duration
        let audio_start_idx = if raw_audio.is_empty() {
            0
        } else {
            let actual_duration = latest - video_frames[0].timestamp;
            let expected_audio_start = raw_audio.last().unwrap().timestamp - actual_duration;
            raw_audio
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    let da = (a.timestamp - expected_audio_start).abs();
                    let db = (b.timestamp - expected_audio_start).abs();
                    da.partial_cmp(&db).unwrap()
                })
                .map(|(i, _)| i)
                .unwrap_or(0)
        };

        // Adjust video timestamps relative to the first video frame
        let video_start_time = video_frames[0].timestamp;
        let mut out_video: Vec<EncodedFrame> = video_meta;
        out_video.extend(video_frames.iter().cloned().map(|f| {
            let ts = f.timestamp - video_start_time;
            f.with_timestamp(ts)
        }));

        // Adjust audio timestamps relative to the first audio frame
        let audio_frames = &raw_audio[audio_start_idx..];
        let mut out_audio: Vec<EncodedFrame> = audio_meta;
        if !audio_frames.is_empty() {
            let audio_start_time = audio_frames[0].timestamp;
            out_audio.extend(audio_frames.iter().cloned().map(|f| {
                let ts = f.timestamp - audio_start_time;
                f.with_timestamp(ts)
            }));
        }

        (out_video, out_audio)
    }

    fn ensure_capacity_locked(&self, g: &mut BufferInner, required: usize) {
        if g.current_memory_bytes + required <= self.max_memory_bytes {
            return;
        }
        let need_to_free = (g.current_memory_bytes + required)
            .saturating_sub(self.max_memory_bytes);
        let mut freed = 0usize;

        while freed < need_to_free {
            match (g.video_queue.front(), g.audio_queue.front()) {
                (Some(v), Some(a)) => {
                    if v.timestamp <= a.timestamp {
                        let f = g.video_queue.pop_front().unwrap();
                        freed += f.data.len();
                        g.current_memory_bytes =
                            g.current_memory_bytes.saturating_sub(f.data.len());
                    } else {
                        let f = g.audio_queue.pop_front().unwrap();
                        freed += f.data.len();
                        g.current_memory_bytes =
                            g.current_memory_bytes.saturating_sub(f.data.len());
                    }
                }
                (Some(_), None) => {
                    let f = g.video_queue.pop_front().unwrap();
                    freed += f.data.len();
                    g.current_memory_bytes =
                        g.current_memory_bytes.saturating_sub(f.data.len());
                }
                (None, Some(_)) => {
                    let f = g.audio_queue.pop_front().unwrap();
                    freed += f.data.len();
                    g.current_memory_bytes =
                        g.current_memory_bytes.saturating_sub(f.data.len());
                }
                (None, None) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(ts: f64, size: usize, kind: SampleKind) -> EncodedFrame {
        EncodedFrame::new(vec![0u8; size], ts, kind)
    }

    fn audio(ts: f64, size: usize) -> EncodedFrame {
        EncodedFrame::new(vec![0u8; size], ts, SampleKind::Interpolated)
    }

    // ── RED: buffer overflow drops oldest frames ─────────────────────────────

    #[test]
    fn drops_oldest_video_frame_when_memory_exceeded() {
        let buf = BoundedEncodedFrameBuffer::new(100);

        // fill to 80 bytes
        buf.try_add_video_frame(video(0.0, 40, SampleKind::Key));
        buf.try_add_video_frame(video(1.0, 40, SampleKind::Interpolated));
        assert_eq!(buf.memory_usage(), 80);

        // adding 40 more (total would be 120) must evict the oldest
        buf.try_add_video_frame(video(2.0, 40, SampleKind::Interpolated));
        assert!(buf.memory_usage() <= 100, "memory_usage={}", buf.memory_usage());
    }

    #[test]
    fn drops_frames_interleaved_by_timestamp() {
        let buf = BoundedEncodedFrameBuffer::new(80);

        buf.try_add_video_frame(video(0.0, 30, SampleKind::Key));
        buf.try_add_audio_frame(audio(0.5, 30));
        // total=60; adding 30 more must evict the oldest (video at t=0)
        buf.try_add_video_frame(video(1.0, 30, SampleKind::Interpolated));

        assert!(buf.memory_usage() <= 80, "memory_usage={}", buf.memory_usage());
    }

    // ── get_frames_for_duration ──────────────────────────────────────────────

    #[test]
    fn returns_empty_when_no_frames() {
        let buf = BoundedEncodedFrameBuffer::new(1024);
        let (v, a) = buf.get_frames_for_duration(None);
        assert!(v.is_empty());
        assert!(a.is_empty());
    }

    #[test]
    fn returns_empty_when_no_keyframe_present() {
        let buf = BoundedEncodedFrameBuffer::new(1024);
        buf.try_add_video_frame(video(0.0, 10, SampleKind::Interpolated));
        let (v, a) = buf.get_frames_for_duration(None);
        assert!(v.is_empty());
        assert!(a.is_empty());
    }

    #[test]
    fn starts_output_from_first_keyframe() {
        let buf = BoundedEncodedFrameBuffer::new(1024);
        buf.try_add_video_frame(video(0.0, 10, SampleKind::Interpolated));
        buf.try_add_video_frame(video(1.0, 10, SampleKind::Key));
        buf.try_add_video_frame(video(2.0, 10, SampleKind::Interpolated));

        let (v, _) = buf.get_frames_for_duration(None);
        // first frame must be a key frame (metadata prepended but here there are none)
        assert_eq!(v.len(), 2, "expected 2 frames (key + interp)");
        assert_eq!(v[0].kind, SampleKind::Key);
        assert_eq!(v[0].timestamp, 0.0, "timestamps re-based to 0");
    }

    #[test]
    fn selects_keyframe_nearest_to_requested_duration() {
        let buf = BoundedEncodedFrameBuffer::new(1024);
        // 5-second recording: keyframes at t=0 and t=3
        for t in [0.0f64, 1.0, 2.0, 3.0, 4.0, 5.0] {
            let kind = if t == 0.0 || t == 3.0 {
                SampleKind::Key
            } else {
                SampleKind::Interpolated
            };
            buf.try_add_video_frame(video(t, 10, kind));
        }

        // request last 2 seconds → latest=5.0, expected_start=3.0 → keyframe at t=3
        let (v, _) = buf.get_frames_for_duration(Some(2.0));
        assert!(!v.is_empty());
        assert_eq!(v[0].kind, SampleKind::Key);
        assert_eq!(v[0].timestamp, 0.0, "first frame re-based to 0");
        // should have 3 frames: t=3,4,5 → timestamps 0,1,2
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn timestamps_rebased_to_zero() {
        let buf = BoundedEncodedFrameBuffer::new(1024);
        buf.try_add_video_frame(video(10.0, 10, SampleKind::Key));
        buf.try_add_video_frame(video(11.0, 10, SampleKind::Interpolated));
        buf.try_add_audio_frame(audio(10.0, 10));
        buf.try_add_audio_frame(audio(10.5, 10));

        let (v, a) = buf.get_frames_for_duration(None);
        assert_eq!(v[0].timestamp, 0.0);
        assert_eq!(v[1].timestamp, 1.0);
        assert_eq!(a[0].timestamp, 0.0);
    }

    #[test]
    fn metadata_frames_prepended_to_output() {
        let buf = BoundedEncodedFrameBuffer::new(1024);
        buf.try_add_video_frame(video(0.0, 10, SampleKind::Metadata));
        buf.try_add_video_frame(video(0.0, 10, SampleKind::Key));
        buf.try_add_video_frame(video(1.0, 10, SampleKind::Interpolated));

        let (v, _) = buf.get_frames_for_duration(None);
        assert_eq!(v[0].kind, SampleKind::Metadata);
        assert_eq!(v[1].kind, SampleKind::Key);
    }
}
