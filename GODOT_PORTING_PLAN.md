# InstantReplay Godot Porting Plan

This document outlines the strategy and roadmap for porting the `InstantReplay` project from Unity to Godot.

## 1. Overview & Objectives

The goal is to provide a high-performance "Instant Replay" (last-N-seconds recording) capability for the Godot Engine.

### Core Objectives:
- **Zero-Copy Capture**: Leverage Godot's `RenderingServer` to access GPU texture handles directly.
- **Minimal Engine Overhead**: Implement the entire pipeline (capture -> encode -> buffer -> mux) in Rust via GDExtension.
- **Engine Agnostic Core**: Decouple the "Instant Replay" logic (ring buffering, GOP management) from Godot-specific code.
- **Robustness**: Use Test-Driven Development (TDD) for the core state machine and buffering logic.

---

## 2. Target Architecture

The Godot version will move away from the Unity C# managed pipeline to a pure Rust GDExtension architecture.

```mermaid
graph TD
    subgraph Godot Engine
        VP[Viewport / RenderingServer]
        AS[AudioServer / AudioEffectCapture]
    end

    subgraph GDExtension (Rust)
        Node[InstantReplayRecorder Node]
        Core[Replay Core Logic - Rust]
        Buffer[Bounded Encoded Buffer - Rust]
    end

    subgraph Native Encoders (unienc)
        Enc[Hardware Video/Audio Encoders]
        Mux[MP4 Muxer]
    end

    VP -->|Native Texture Handle| Node
    AS -->|PCM Buffer| Node
    Node --> Core
    Core --> Enc
    Enc --> Buffer
    Buffer --> Mux
```

---

## 3. Implementation Phases

### Phase 1: Foundation & Core Logic (Rust TDD)
*Goal: Reimplement the C# logic from `InstantReplay.Runtime` into the Rust `unienc` workspace.*

1.  **Temporal Adjuster**: Port the logic that handles frame rate stabilization and AV sync.
2.  **Bounded Ring Buffer**: Implement a thread-safe ring buffer that stores encoded `H.264` GOPs and `AAC` packets.
    - *TDD Focus*: Verify packet dropping when the memory limit is reached.
3.  **Session Controller**: A state machine managing `Recording`, `Paused`, and `Exporting` states.

### Phase 2: GDExtension Wrapper
*Goal: Expose the Rust logic to Godot as a native Node.*

1.  **Project Setup**: Initialize `godot-rust` (GDExtension) in the `InstantReplay.Externals/unienc` workspace.
2.  **`InstantReplayRecorder` Node**:
    - Properties: `max_duration`, `bitrate`, `output_path`, `max_memory_usage`.
    - Methods: `start()`, `stop()`, `pause()`, `export_replay(seconds)`.
    - Signals: `export_completed(path)`, `error_occurred(message)`.

### Phase 3: Hardware Capture Integration
*Goal: Hook into Godot's rendering and audio systems.*

1.  **Video Capture**:
    - Connect to `RenderingServer.frame_post_draw`.
    - Retrieve the RID of the target Viewport.
    - Get the native texture handle (Vulkan Image / D3D Texture).
2.  **Audio Capture**:
    - Implement a mechanism to pull samples from an `AudioEffectCapture` on the Master bus.
3.  **Synchronization**: Ensure captured frames and audio samples are timestamped using `Time.get_ticks_usec()`.

### Phase 4: Platform-Specific Optimization
*Goal: Ensure low-latency native interop.*

1.  **Vulkan Interop**: (Windows/Linux) Optimize the transfer of Vulkan images to the native encoders (MediaFoundation/VAAPI).
2.  **Mobile Support**: Ensure `MediaCodec` (Android) and `VideoToolbox` (iOS) work within the Godot environment.

---

## 4. Testing Strategy

### Unit Testing (Rust)
Run these during development to ensure logic correctness:
```bash
cargo test -p unienc_core
```
- Test buffer overflow scenarios.
- Test audio/video interleaving logic.
- Test temporal drift correction.

### Integration Testing (Godot)
- **Visual Validation**: Create a simple Godot scene with a moving object and verify the exported MP4 matches.
- **Stress Test**: Run a 1-hour recording session and monitor memory usage to ensure the "Bounded" buffer is actually bounding memory.

---

## 5. Directory Structure Changes

We will introduce a new crate for Godot:
- `InstantReplay.Externals/unienc/crates/unienc_godot/`: The GDExtension entry point.
- `InstantReplay.Externals/unienc/crates/unienc_core/`: Engine-agnostic replay logic (extracted from Unity C#).

---

## 6. Execution Instructions for Sandbox

1.  **Launch Sandbox**: `gemini -s` (Docker backend).
2.  **Build Rust**: `cargo build` to ensure the current `unienc` workspace is healthy.
3.  **Initialize Godot Crate**:
    ```bash
    cd InstantReplay.Externals/unienc/crates
    cargo new unienc_godot --lib
    ```
4.  **Follow Phase 1**: Start by moving the `BoundedEncodedFrameBuffer.cs` logic into `unienc_core`.
