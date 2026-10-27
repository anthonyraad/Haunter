//! `cargo run --release --features standalone --example standalone -- --backend dummy`
//!
//! Set `HAUNTER_PREVIEW_SAMPLE` to a WAV, AIFF, or MP3 path to load it at startup.

use nih_plug::prelude::*;

fn main() {
    nih_export_standalone::<haunter::Haunter>();
}
