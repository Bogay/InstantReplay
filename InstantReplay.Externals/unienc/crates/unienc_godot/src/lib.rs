use godot::prelude::*;

pub mod pipeline;
mod recorder;

struct InstantReplayExtension;

#[gdextension]
unsafe impl ExtensionLibrary for InstantReplayExtension {}
