use godot::prelude::*;

mod pipeline;
mod recorder;

struct InstantReplayExtension;

#[gdextension]
unsafe impl ExtensionLibrary for InstantReplayExtension {}
