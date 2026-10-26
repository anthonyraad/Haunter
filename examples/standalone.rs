//! `cargo run --release --features standalone --example standalone -- --backend dummy`
//!
//! Set `GHOSTPUMP_PREVIEW_SAMPLE` to a WAV or AIFF path to load it at startup.

use nih_plug::prelude::*;

fn main() {
    nih_export_standalone::<ghostpump::Ghostpump>();
}
