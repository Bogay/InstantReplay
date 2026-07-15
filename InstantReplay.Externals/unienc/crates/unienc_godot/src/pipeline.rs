use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use bincode::config::standard;
use tokio::sync::mpsc;
use unienc::{
    AudioSample, CompletionHandle, EncodedData, Encoder, EncoderInput, EncoderOutput,
    EncodingSystem, Muxer, MuxerInput, Runtime as RuntimeTrait, Spawn, SpawnBlocking,
    UniencSampleKind, VideoFrame, VideoFrameBgra32, VideoSample, buffer::SharedBuffer,
};
use unienc_core::buffer::{BoundedEncodedFrameBuffer, EncodedFrame, SampleKind};

// ── Pipeline construction options ───────────────────────────────────────────

/// Converts an `audio_input_queue_size_seconds` value to a bounded-channel capacity.
/// Uses 512 samples/chunk as a typical Godot audio buffer size.
const GODOT_AUDIO_SAMPLES_PER_CHUNK: usize = 512;

fn compute_audio_queue_capacity(sample_rate: u32, seconds: f64) -> usize {
    let raw = (sample_rate as f64 * seconds / GODOT_AUDIO_SAMPLES_PER_CHUNK as f64) as usize;
    raw.max(4)
}

/// Controls the sizes of the bounded input channels feeding the encoding pipeline.
#[must_use]
pub struct PipelineOptions {
    /// Max queued video frames before `try_send_video` starts dropping.
    pub video_input_queue_size: usize,
    /// Max queued audio frames before `try_send_audio` starts dropping.
    pub audio_input_queue_size: usize,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        Self {
            video_input_queue_size: 32,
            audio_input_queue_size: compute_audio_queue_capacity(44_100, 1.0),
        }
    }
}

impl PipelineOptions {
    /// Build options from user-facing configuration. `audio_sample_rate` is used to convert
    /// `audio_input_queue_size_seconds` into a bounded-channel capacity.
    pub fn from_config(
        video_input_queue_size: usize,
        audio_input_queue_size_seconds: f64,
        audio_sample_rate: u32,
    ) -> Self {
        Self {
            video_input_queue_size: video_input_queue_size.max(1),
            audio_input_queue_size: compute_audio_queue_capacity(
                audio_sample_rate,
                audio_input_queue_size_seconds,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GODOT_AUDIO_SAMPLES_PER_CHUNK, PipelineOptions, compute_audio_queue_capacity};

    #[test]
    fn pipeline_options_default_has_sensible_queue_sizes() {
        let opts = PipelineOptions::default();
        assert_eq!(opts.video_input_queue_size, 32);
        assert!(
            opts.audio_input_queue_size >= 4,
            "audio queue too small: {}",
            opts.audio_input_queue_size
        );
    }

    #[test]
    fn audio_queue_capacity_scales_with_seconds() {
        let cap_1s = compute_audio_queue_capacity(44_100, 1.0);
        let cap_2s = compute_audio_queue_capacity(44_100, 2.0);
        assert_eq!(cap_2s, cap_1s * 2, "2s should give 2× the capacity of 1s");
    }

    #[test]
    fn audio_queue_capacity_minimum_is_four() {
        // Very short duration must not produce a pathologically small channel.
        let cap = compute_audio_queue_capacity(44_100, 0.0);
        assert!(cap >= 4, "minimum must be ≥4, got {cap}");
    }
}

// ── Encoder options ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct GodotVideoOptions {
    pub width: u32,
    pub height: u32,
    pub fps_hint: u32,
    pub bitrate: u32,
}

impl unienc::VideoEncoderOptions for GodotVideoOptions {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn fps_hint(&self) -> u32 {
        self.fps_hint
    }
    fn bitrate(&self) -> u32 {
        self.bitrate
    }
}

#[derive(Clone, Copy)]
pub struct GodotAudioOptions {
    pub sample_rate: u32,
    pub channels: u32,
    pub bitrate: u32,
}

impl unienc::AudioEncoderOptions for GodotAudioOptions {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u32 {
        self.channels
    }
    fn bitrate(&self) -> u32 {
        self.bitrate
    }
}

// ── Tokio-backed unienc::Runtime ────────────────────────────────────────────

#[derive(Clone)]
pub struct TokioRuntime {
    handle: tokio::runtime::Handle,
}

impl Spawn for TokioRuntime {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.handle.spawn(future);
    }
}

impl SpawnBlocking for TokioRuntime {
    fn spawn_blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce() -> R + Send + 'static,
    ) -> Pin<Box<dyn Future<Output = R> + Send + 'static>> {
        let handle = self.handle.clone();
        Box::pin(async move { handle.spawn_blocking(f).await.unwrap() })
    }
}

impl RuntimeTrait for TokioRuntime {}

// ── Platform type aliases ────────────────────────────────────────────────────

type GodotSystem =
    unienc::PlatformEncodingSystem<GodotVideoOptions, GodotAudioOptions, TokioRuntime>;
type GodotVideoEnc = <GodotSystem as EncodingSystem>::VideoEncoderType;
type GodotVideoIn = <GodotVideoEnc as Encoder>::InputType;
type GodotVideoOut = <GodotVideoEnc as Encoder>::OutputType;
type GodotVideoData = <GodotVideoOut as EncoderOutput>::Data;
type GodotAudioEnc = <GodotSystem as EncodingSystem>::AudioEncoderType;
type GodotAudioIn = <GodotAudioEnc as Encoder>::InputType;
type GodotAudioOut = <GodotAudioEnc as Encoder>::OutputType;
type GodotAudioData = <GodotAudioOut as EncoderOutput>::Data;
type GodotMux = <GodotSystem as EncodingSystem>::MuxerType;
type GodotVideoMuxIn = <GodotMux as Muxer>::VideoInputType;
type GodotAudioMuxIn = <GodotMux as Muxer>::AudioInputType;
type GodotMuxCompletion = <GodotMux as Muxer>::CompletionHandleType;

// ── Raw frame types (channel payload) ───────────────────────────────────────

pub struct VideoRawFrame {
    pub bgra32: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp: f64,
}

pub struct AudioRawFrame {
    pub samples_i16: Vec<i16>,
    pub timestamp_in_samples: u64,
}

// ── EncodingPipeline ─────────────────────────────────────────────────────────

pub struct EncodingPipeline {
    pub video_tx: Option<mpsc::Sender<VideoRawFrame>>,
    pub audio_tx: Option<mpsc::Sender<AudioRawFrame>>,
    pub buffer: Arc<BoundedEncodedFrameBuffer>,
    encoding_system: GodotSystem,
    tokio_rt: tokio::runtime::Runtime,
    video_push_handle: Option<tokio::task::JoinHandle<()>>,
    video_pull_handle: Option<tokio::task::JoinHandle<()>>,
    audio_push_handle: Option<tokio::task::JoinHandle<()>>,
    audio_pull_handle: Option<tokio::task::JoinHandle<()>>,
}

impl EncodingPipeline {
    pub fn new(
        video_opts: GodotVideoOptions,
        audio_opts: GodotAudioOptions,
        max_memory_bytes: usize,
        pipeline_opts: PipelineOptions,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let tokio_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;

        let runtime = TokioRuntime {
            handle: tokio_rt.handle().clone(),
        };
        let encoding_system = GodotSystem::new(&video_opts, &audio_opts, runtime);

        // new_video_encoder() prints "Running FFmpeg" and spawns the ffmpeg
        // subprocess via tokio::process::Command. That call needs a reactor on
        // the current thread — enter the runtime with block_on first.
        let ((video_in, video_out), (audio_in, audio_out)) = tokio_rt.block_on(async {
            let video_encoder = encoding_system.new_video_encoder()?;
            let audio_encoder = encoding_system.new_audio_encoder()?;
            let vp = video_encoder.get()?;
            let ap = audio_encoder.get()?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((vp, ap))
        })?;

        let buffer = Arc::new(BoundedEncodedFrameBuffer::new(max_memory_bytes));

        let (video_tx, video_rx) =
            mpsc::channel::<VideoRawFrame>(pipeline_opts.video_input_queue_size);
        let (audio_tx, audio_rx) =
            mpsc::channel::<AudioRawFrame>(pipeline_opts.audio_input_queue_size);

        let vbuf = Arc::clone(&buffer);
        let abuf = Arc::clone(&buffer);

        let video_push_handle = tokio_rt.spawn(video_push_task(video_rx, video_in));
        let video_pull_handle = tokio_rt.spawn(video_pull_task(video_out, vbuf));
        let audio_push_handle = tokio_rt.spawn(audio_push_task(audio_rx, audio_in));
        let audio_pull_handle = tokio_rt.spawn(audio_pull_task(audio_out, abuf));

        Ok(Self {
            video_tx: Some(video_tx),
            audio_tx: Some(audio_tx),
            buffer,
            encoding_system,
            tokio_rt,
            video_push_handle: Some(video_push_handle),
            video_pull_handle: Some(video_pull_handle),
            audio_push_handle: Some(audio_push_handle),
            audio_pull_handle: Some(audio_pull_handle),
        })
    }

    pub fn try_send_video(&self, frame: VideoRawFrame) -> bool {
        self.video_tx
            .as_ref()
            .map_or(false, |tx| tx.try_send(frame).is_ok())
    }

    pub fn try_send_audio(&self, frame: AudioRawFrame) -> bool {
        self.audio_tx
            .as_ref()
            .map_or(false, |tx| tx.try_send(frame).is_ok())
    }

    /// Drop input channels → encoders receive EOF → pull tasks drain → return.
    pub fn stop(&mut self) {
        self.video_tx = None;
        self.audio_tx = None;

        let vph = self.video_push_handle.take();
        let vplh = self.video_pull_handle.take();
        let aph = self.audio_push_handle.take();
        let aplh = self.audio_pull_handle.take();

        self.tokio_rt.block_on(async move {
            if let Some(h) = vph {
                let _ = h.await;
            }
            if let Some(h) = vplh {
                let _ = h.await;
            }
            if let Some(h) = aph {
                let _ = h.await;
            }
            if let Some(h) = aplh {
                let _ = h.await;
            }
        });
    }

    /// Drain the buffer and mux to the given output path.
    pub fn export_to_file(
        &self,
        duration_secs: Option<f64>,
        output_path: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (video_frames, audio_frames) = self.buffer.get_frames_for_duration(duration_secs);

        // new_muxer() spawns the ffmpeg muxer process and wraps its stdio as
        // tokio async pipes — both require a reactor. Enter the runtime first.
        let encoding_system = &self.encoding_system;
        let output_path = output_path.to_owned();
        self.tokio_rt.block_on(async move {
            let muxer = encoding_system.new_muxer(Path::new(&output_path))?;
            let (mut vmux, mut amux, completion): (
                GodotVideoMuxIn,
                GodotAudioMuxIn,
                GodotMuxCompletion,
            ) = muxer.get_inputs()?;

            // Push video and audio concurrently so neither pipe can deadlock
            // the other. The MP4 muxer needs data from both inputs to produce
            // interleaved output; writing them sequentially would deadlock once
            // the video pipe buffer fills while the mux waits for audio.
            let video_fut = async {
                for frame in &video_frames {
                    let (mut data, _): (GodotVideoData, _) =
                        bincode::decode_from_slice(&frame.data, standard())?;
                    data.set_timestamp(frame.timestamp);
                    vmux.push(data).await?;
                }
                vmux.finish().await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            };

            let audio_fut = async {
                for frame in &audio_frames {
                    let (mut data, _): (GodotAudioData, _) =
                        bincode::decode_from_slice(&frame.data, standard())?;
                    data.set_timestamp(frame.timestamp);
                    amux.push(data).await?;
                }
                amux.finish().await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            };

            let (vr, ar) = tokio::join!(video_fut, audio_fut);
            vr?;
            ar?;

            completion.finish().await?;

            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
    }
}

// ── Background encoding tasks ────────────────────────────────────────────────

async fn video_push_task(mut rx: mpsc::Receiver<VideoRawFrame>, mut input: GodotVideoIn) {
    while let Some(raw) = rx.recv().await {
        let sample = VideoSample {
            frame: VideoFrame::Bgra32(VideoFrameBgra32 {
                buffer: SharedBuffer::new_unmanaged(raw.bgra32),
                width: raw.width,
                height: raw.height,
            }),
            timestamp: raw.timestamp,
        };
        if input.push(sample).await.is_err() {
            break;
        }
    }
    // input dropped here → ffmpeg stdin closed → EOF signaled
}

async fn video_pull_task(mut output: GodotVideoOut, buffer: Arc<BoundedEncodedFrameBuffer>) {
    loop {
        match output.pull().await {
            Ok(Some(data)) => {
                let kind = map_kind(data.kind());
                let ts = data.timestamp();
                let bytes = bincode::encode_to_vec(&data, standard()).unwrap_or_default();
                buffer.try_add_video_frame(EncodedFrame::new(bytes, ts, kind));
            }
            Ok(None) | Err(_) => break,
        }
    }
}

async fn audio_push_task(mut rx: mpsc::Receiver<AudioRawFrame>, mut input: GodotAudioIn) {
    while let Some(raw) = rx.recv().await {
        let sample = AudioSample {
            data: raw.samples_i16,
            timestamp_in_samples: raw.timestamp_in_samples,
        };
        if input.push(sample).await.is_err() {
            break;
        }
    }
}

async fn audio_pull_task(mut output: GodotAudioOut, buffer: Arc<BoundedEncodedFrameBuffer>) {
    loop {
        match output.pull().await {
            Ok(Some(data)) => {
                let kind = map_kind(data.kind());
                let ts = data.timestamp();
                let bytes = bincode::encode_to_vec(&data, standard()).unwrap_or_default();
                buffer.try_add_audio_frame(EncodedFrame::new(bytes, ts, kind));
            }
            Ok(None) | Err(_) => break,
        }
    }
}

fn map_kind(k: UniencSampleKind) -> SampleKind {
    match k {
        UniencSampleKind::Key => SampleKind::Key,
        UniencSampleKind::Interpolated => SampleKind::Interpolated,
        UniencSampleKind::Metadata => SampleKind::Metadata,
    }
}
