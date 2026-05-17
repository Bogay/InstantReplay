use unienc_godot::pipeline::{
    AudioRawFrame, EncodingPipeline, GodotAudioOptions, GodotVideoOptions, VideoRawFrame,
};

const W: u32 = 160;
const H: u32 = 120;
const FPS: u32 = 10;
const FRAMES: usize = 20; // 2 seconds

fn video_opts() -> GodotVideoOptions {
    GodotVideoOptions { width: W, height: H, fps_hint: FPS, bitrate: 1_000_000 }
}

fn audio_opts() -> GodotAudioOptions {
    GodotAudioOptions { sample_rate: 44_100, channels: 2, bitrate: 64_000 }
}

fn send_frames(pipeline: &EncodingPipeline) {
    let frame_pixels = (W * H * 4) as usize;
    let samples_per_frame = 44_100 / FPS as u64;

    for i in 0..FRAMES {
        let ts = i as f64 / FPS as f64;

        // Alternating BGRA red/blue frames
        let b: u8 = if i % 2 == 0 { 0xff } else { 0x00 };
        let mut bgra = vec![0u8; frame_pixels];
        for chunk in bgra.chunks_mut(4) {
            chunk[0] = b;
            chunk[2] = 0xff - b;
            chunk[3] = 0xff;
        }
        pipeline.try_send_video(VideoRawFrame { bgra32: bgra, width: W, height: H, timestamp: ts });

        let ts_samples = i as u64 * samples_per_frame;
        pipeline.try_send_audio(AudioRawFrame {
            samples_i16: vec![0i16; samples_per_frame as usize * 2],
            timestamp_in_samples: ts_samples,
        });
    }
}

#[test]
fn pipeline_encodes_and_exports_full_duration() {
    let mut pipeline =
        EncodingPipeline::new(video_opts(), audio_opts(), 32 * 1024 * 1024).expect("init");
    send_frames(&pipeline);
    pipeline.stop();

    let out = "/tmp/unienc_e2e_full.mp4";
    pipeline.export_to_file(None, out).expect("export");

    let size = std::fs::metadata(out).expect("file missing").len();
    assert!(size > 0, "output file is empty");
    std::fs::remove_file(out).ok();
}

#[test]
fn pipeline_trims_to_requested_duration() {
    let mut pipeline =
        EncodingPipeline::new(video_opts(), audio_opts(), 32 * 1024 * 1024).expect("init");
    send_frames(&pipeline);
    pipeline.stop();

    let out_full = "/tmp/unienc_e2e_trim_full.mp4";
    let out_trimmed = "/tmp/unienc_e2e_trim_1s.mp4";
    pipeline.export_to_file(None, out_full).expect("full export");
    pipeline.export_to_file(Some(1.0), out_trimmed).expect("trimmed export");

    let full_size = std::fs::metadata(out_full).expect("full file missing").len();
    let trimmed_size = std::fs::metadata(out_trimmed).expect("trimmed file missing").len();
    assert!(trimmed_size < full_size, "trimmed file should be smaller than full");

    std::fs::remove_file(out_full).ok();
    std::fs::remove_file(out_trimmed).ok();
}

#[test]
fn pipeline_respects_memory_ceiling() {
    // Tiny ceiling — frames must be evicted
    let ceiling = 50_000;
    let mut pipeline =
        EncodingPipeline::new(video_opts(), audio_opts(), ceiling).expect("init");
    send_frames(&pipeline);
    pipeline.stop();

    // Should still export without error even though most frames were dropped
    let out = "/tmp/unienc_e2e_ceiling.mp4";
    pipeline.export_to_file(None, out).expect("export with ceiling");
    std::fs::remove_file(out).ok();
}
