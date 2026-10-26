# Ghostpump: build brief

Build a Windows 64-bit **VST3 audio effect** (not an instrument) for Cubase called **Ghostpump**. The user loads a sample (typically a kick or any one-shot), and the sample's volume shape ducks the incoming track, like a ghost-kick sidechain. The sample can also be blended into the output.

Follow the same Rust approach as `..\Sparkletop\plugin` (look at its Cargo.toml, .cargo config, static CRT, and VST3 bundle layout) and the knob look of the Dropout plugin (`..\Sparkletop` commit 56acf79 copied Dropout's disc, track, and blue arc knobs; reuse that style).

## Behaviour
- **Trigger:** synced to Cubase's tempo and transport. Rate choices are 1/1, 1/2, 1/4, 1/8, 1/16, plus dotted and triplet variants. Retrigger on the grid only while the transport plays.
- **Sample region:** Start and End times within the sample. Show the waveform in the UI with draggable start and end handles.
- **Playback mode:** One-shot (play the region once per trigger) or Loop (loop the region continuously, restarting on each trigger).
- **Ducking:** an envelope follower runs on the sample's playback (after the sidechain filter). Its shape sets gain reduction on the input. Controls are Depth (0 to 100%, how much the track ducks at the sample's peak), Attack and Release smoothing for the follower, and Invert (optional).
- **Sidechain filter:** HP and LP cutoffs on the sample signal before the follower, so you can key from just the kick's low end.
- **Sample output:** Sample Volume, plus Sample Blend (how much of the sample is mixed into the output, default 0, so it's only a ghost).
- **Mix:** dry/wet for the ducking effect. **Output gain.**
- **Gain-reduction meter** in the UI.
- **Loading:** drag-and-drop a WAV/AIFF onto the plugin window, plus a Load button. Resample to the host rate.
- **State:** the sample audio itself is embedded in the plugin state, so a Cubase project or preset reopens with the sample even if the original file moved.
- **Latency:** none needed (no lookahead).

## Deliverables
1. Source in this repo, with DSP unit tests (envelope follower, tempo-grid trigger timing, one-shot vs loop, start/end, state save/restore with the embedded sample).
2. `cargo build --release` produces the DLL. Package it as `release\Ghostpump.vst3\Contents\x86_64-win\Ghostpump.vst3` with the proper bundle layout.
3. README with the controls and the install steps.
4. Commit when it builds and the tests pass.
