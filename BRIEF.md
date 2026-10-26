# Ghostpump: build brief

Build a Windows 64-bit **VST3 audio effect** (not an instrument) for Cubase called **Ghostpump**. The user loads a sample (typically a kick or any one-shot), and the sample's volume shape ducks the incoming track, like a ghost-kick sidechain. The sample can also be blended into the output.

Follow the same Rust approach as `..\Sparkletop\plugin` (look at its Cargo.toml, .cargo config, static CRT, and VST3 bundle layout) and the knob look of the Dropout plugin (`..\Sparkletop` commit 56acf79 copied Dropout's disc, track, and blue arc knobs; reuse that style).

## Behaviour
- **Trigger:** synced to Cubase's tempo and transport. Rate choices are 1/1, 1/2, 1/4, 1/8, 1/16, plus dotted and triplet variants. Retrigger on the grid only while the transport plays.
- **Sample region:** Start and End times within the sample. Show the waveform in the UI with draggable start and end handles.
- **Restart** toggle, on by default. On: the region plays once from Start on every Rate grid step. Off: the Rate knob is greyed out and disabled, and the region loops continuously while the transport plays. Old states map One-shot to Restart on and Loop to Restart off.
- **Reverse** toggle: plays the region backwards, from End to Start. The ducking follows the reversed shape.
- **Ducking:** an envelope follower runs on the sample's playback (after the HP and LP filters). Its shape sets gain reduction on the input. Controls are Depth (0 to 100%, how much the track ducks at the unfiltered sample's peak), and Attack and Release smoothing for the follower.
- **Filters:** HP and LP cutoffs filter the sample itself, so they change both the sample heard through Sample Blend and the ducking. Filtering away part of the sample makes the duck shallower.
- **Sample output:** Sample Volume (default 0.0 dB), plus Sample Blend (how much of the sample is mixed into the output, default 0, so it's only a ghost).
- **Mix:** dry/wet for the ducking effect. **Output gain.**
- **Gain-reduction meter** in the UI.
- **Loading:** drag-and-drop a WAV/AIFF onto the plugin window, plus a Load button. Resample to the host rate.
- **State:** the sample audio itself is embedded in the plugin state, so a Cubase project or preset reopens with the sample even if the original file moved.
- **Latency:** none needed (no lookahead).

## Deliverables
1. Source in this repo, with DSP unit tests (envelope follower, tempo-grid trigger timing, Restart on vs off, Reverse, HP and LP changing the output, start/end, state save/restore with the embedded sample, loading old states).
2. `cargo build --release` produces the DLL. Package it as `release\Ghostpump.vst3\Contents\x86_64-win\Ghostpump.vst3` with the proper bundle layout.
3. README with the controls and the install steps.
4. Commit when it builds and the tests pass.
