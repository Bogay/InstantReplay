use godot::prelude::*;

mod recorder;

struct InstantReplayExtension;

#[gdextension]
unsafe impl ExtensionLibrary for InstantReplayExtension {}
