# Ghostpump

Ghostpump is a 64-bit Windows VST3 audio effect for Cubase. You load a sample, usually a kick or another one-shot. On every beat of a tempo-synced grid, the sample's volume shape ducks the track Ghostpump is inserted on, like sidechaining to a ghost kick. You can also blend the sample into the output, but by default you only hear the ducking.

The plugin is `release/Ghostpump.vst3`.

## Install in Cubase

1. Copy the whole `release\Ghostpump.vst3` folder to `C:\Program Files\Common Files\VST3\`. You need administrator rights for that folder.
2. In Cubase, open Studio > VST Plug-in Manager and rescan, or restart Cubase.
3. Insert Ghostpump on an audio, group, or FX channel. It is listed under Dynamics.
4. Drag a WAV or AIFF onto the plugin window, or click **Load**, then press play.

The plugin works in stereo or mono and adds no latency. The C runtime is linked statically, so nothing else needs installing.

## How it works

With **Restart** on (the default), Ghostpump plays the selected region once from Start on every line of the tempo grid while the transport plays. Grid lines are counted from the start of the project. With Restart off, the Rate knob is greyed out and the region loops continuously from the moment the transport starts. Either way, nothing starts while the transport is stopped, and a pass that is already playing finishes, then stops.

The HP and LP filters work on the sample itself as it plays. An envelope follower runs on the filtered sample, and its level sets how far the track is turned down. Depth is measured against the unfiltered sample's peak over the region: with the filters off, the track ducks by exactly Depth at the sample's loudest point. Filtering part of the sample away makes the duck shallower, and changes what you hear through Blend.

**Dream Eater** puts a resonant comb filter on the track, and the same envelope that drives the ducking (after Reverse, HP, and LP) plays it. Between hits the comb rests at a 9 ms delay, with its lowest resonance near 111 Hz, and light feedback. On each hit the delay shortens by up to three octaves and the feedback rises, so the resonance sweeps up with the sample and falls back as it decays. Turning the knob up raises the comb's level in the mix, its feedback, and how far it sweeps, all together. At fully counter-clockwise it reads Off and the comb is bypassed completely, so the track is untouched. The feedback never reaches 1, the comb's input is scaled down as the feedback rises, and its loop is damped above 7 kHz. That keeps the level close to the dry track at any setting. The comb runs before the ducking, and the blended sample doesn't go through it.

The output is always at unity gain. There is no output level control.

## Controls

| Control | What it does |
| --- | --- |
| Waveform | Shows the loaded sample. Drag the **S** and **E** handles to set the region. The orange line is the playhead. |
| Load | Opens a file picker for WAV or AIFF. You can also drag a file onto the window. |
| Restart | On by default. On: the region plays once from Start on every Rate step. Off: the region loops continuously while the transport plays, and Rate is disabled. |
| Reverse | Plays the region backwards, from E to S. The ducking follows the reversed shape. |
| Gain reduction | Shows how far the track is turned down by the ducking, in dB. |
| Rate | Grid spacing: 1/1, 1/2, 1/4, 1/8, 1/16, each also dotted or triplet. The default is 1/4. Only used while Restart is on. |
| Depth | 0–100%. How far the track ducks at the sample's peak. The default is 75%. |
| Attack | 0.1–100 ms. How quickly the follower rises. Longer values soften the start of the duck. |
| Release | 5–1000 ms. How quickly the follower falls. Longer values make the track recover more slowly. |
| Mix | Dry/wet for the ducking. At 50%, the track ducks half as far. |
| HP | High-pass on the sample, 20 Hz (Off) to 2 kHz. Affects both the ducking and the blended sample. Raise it on a kick to make the duck shallower. |
| LP | Low-pass on the sample, 50 Hz to 20 kHz (Off). Affects both the ducking and the blended sample. |
| Sample Vol | The sample's level, −48 to +12 dB. The default is 0.0 dB. |
| Blend | How much of the sample is added to the output. The default is 0%, so you only hear the duck. |
| Dream Eater | Off (the default) to 100%, in 1% steps. How hard the sample plays a comb filter on the track. The dark knob shows the sample's live sweep as a blue fill inside its white arc. |

Start and End are automatable parameters too. They show times in milliseconds, measured within the loaded sample. Double-click a knob to reset it. Hold Shift while dragging for fine adjustment.

Projects saved with version 1.0 load with One-shot mode as Restart on and Loop mode as Restart off. Invert no longer exists, so it is dropped when an old project loads. Projects saved with 1.1 or earlier load with Dream Eater Off. Their Output setting is ignored, so a project that used Output to change the level plays at unity gain.

## Samples and project state

- Accepted files: WAV (8, 16, 24, or 32-bit PCM, and 32 or 64-bit float, including WAVE_FORMAT_EXTENSIBLE) and AIFF/AIFC (8–32-bit PCM, `sowt`, `fl32`, `fl64`). Only the first two channels are used. A mono file plays on both sides.
- The sample is resampled to Cubase's sample rate with a windowed-sinc filter. The original sample is kept, so if you change the project's sample rate it is converted again from the original.
- The sample audio itself is saved in the plugin state, inside the Cubase project or preset. The project reopens with the sample even if the original file has been moved or deleted. The audio is stored as base64-encoded 32-bit float, about 470 KB per second of stereo at 44.1 kHz. Files longer than 30 seconds are cut to 30 seconds so projects stay a reasonable size.

## Build from source

You need Rust with the MSVC toolchain and Visual Studio's C++ build tools.

```powershell
cargo test --release
.\scripts\package.ps1
```

`package.ps1` runs `cargo build --release` and copies `target\release\ghostpump.dll` to `release\Ghostpump.vst3\Contents\x86_64-win\Ghostpump.vst3`.

The unit tests cover:

- The envelope follower's attack and release times.
- Grid timing for straight, dotted, and triplet rates, including transport start, cycle jumps, and block-size independence.
- Restart on versus off, Reverse, the Start and End region, and Depth and Mix.
- HP and LP measurably changing both the blended sample and the ducking.
- Dream Eater: Off is a bit-exact bypass, even right after it was on. Turning it up changes the track more. The comb sweeps on the sample's hits and lets go between them. At full, with full-scale noise and a full-scale tone on the comb's resonance, the output stays finite, below +6 dB, and dies away in silence.
- WAV and AIFF decoding and resampling.
- Saving and restoring the state with the embedded sample, default values, and loading 1.0 and 1.1 states.
- Headless editor checks: everything fits the window, simulated clicks on Restart and Reverse reach the host, the Rate knob ignores input while disabled, and Dream Eater sits in Output's old place with its dark face, reads Off, and turns.

`cargo run --release --features standalone --example standalone -- --backend dummy` opens the editor without a host. Set `GHOSTPUMP_PREVIEW_SAMPLE` to a WAV path to load it at startup.

Upstream egui-baseview ignores file drops. `vendor/egui-baseview` is a copy of the revision nih-plug uses, patched to pass dropped files to egui. See `vendor/egui-baseview/PATCHED.md`.
