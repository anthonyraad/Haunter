# Haunter: build brief

Build a Windows 64-bit **VST3 audio effect** (not an instrument) for Cubase called **Haunter**. The user loads a sample (typically a kick or any one-shot), and the sample's volume shape ducks the incoming track, like a ghost-kick sidechain. The sample can also be blended into the output.

Follow the same Rust approach as `..\Sparkletop\plugin` (look at its Cargo.toml, .cargo config, static CRT, and VST3 bundle layout) and the knob look of the Dropout plugin (`..\Sparkletop` commit 56acf79 copied Dropout's disc, track, and blue arc knobs; reuse that style).

## Behaviour
- **Trigger:** synced to Cubase's tempo and transport. Rate choices are 1/1, 1/2, 1/4, 1/8, 1/16, plus dotted and triplet variants. Retrigger on the grid only while the transport plays.
- **Sample region:** Start and End times within the sample. Show the waveform in the UI with draggable start and end handles.
- **Restart** toggle, on by default. On: the region plays once from Start on every Rate grid step. Off: the region loops continuously while the transport plays, and the Rate knob keeps its normal look but ignores dragging, scrolling, and double-clicks, with its readout replaced by a grey ∞ (U+221E, from the font if it has the glyph, drawn otherwise). Turning Restart back on shows the fraction again. Old states map One-shot to Restart on and Loop to Restart off.
- **Reverse** toggle: plays the region backwards, from End to Start. The ducking follows the reversed shape.
- **Ducking:** an envelope follower runs on the sample's playback (after the HP and LP filters). Its shape sets gain reduction on the input. Controls are Depth (0 to 100%, how much the track ducks at the unfiltered sample's peak), and Attack and Release smoothing for the follower.
- **Filters:** HP and LP cutoffs filter the sample itself, so they change both the sample heard through Sample Blend and the ducking. Filtering away part of the sample makes the duck shallower.
- **Sample output:** Sample Volume (default 0.0 dB), plus Sample Blend (how much of the sample is mixed into the output, default 0, so it's only a ghost).
- **Mix:** dry/wet for the ducking effect. There is no output gain: the output is always at unity. Old states that saved Output still load, and the value is ignored.
- **Dream Eater:** a knob in Output's old place in the layout (the last knob of the lower row). The sample's envelope, after Reverse, HP, and LP and following the same playback as the ducking, sweeps the delay time and feedback of a comb filter on the incoming track. The result is a resonant comb that pumps in time with the sample. Fully counter-clockwise is Off, a bit-exact bypass, and the readout says "Off". Turning it clockwise raises the wet amount, feedback, and modulation depth together. The default is Off, and states without it load with it Off. It must be safe: feedback stays below 1, there are no clicks, parameters are smoothed, and levels stay sane.
- **Dream Eater look:** drawn like the knob in Dropout's Destroy section (The End, in `C:\Users\antho\Dropout`): same size and label style as the other knobs, with a dark grey disc, dark outline and track, a white value arc, a thicker white pointer, and a blue fill along the white arc showing the live amount. The other knobs keep their look.
- **Gain-reduction meter** in the UI.
- **Loading:** drag-and-drop a WAV/AIFF onto the plugin window, plus a Load button. Resample to the host rate.
- **State:** the sample audio itself is embedded in the plugin state, so a Cubase project or preset reopens with the sample even if the original file moved.
- **Presets:** a preset dropdown left of a **Save** button, left of **Load**. `--` means no sample and default settings. The factory preset **Water** loads `presets\Water.wav`, embedded in the plugin binary with `include_bytes!`, with the default settings, and the sample is named "Water". Save asks for a name inline (a text field with OK and Cancel, no native dialog) and stores the sample's audio and every parameter value in `%APPDATA%\Haunter\Presets`, one JSON file per preset. Saved presets are listed after the factory ones and persist across sessions. Saving under an existing name overwrites it.
- **Modified indicator:** when any parameter, Start or End, Restart or Reverse, or the sample differs from the selected preset, the name shows a trailing `*` (`Water*`). Choosing the preset again or saving clears it. The selected preset and what it set are saved in the plugin state, so a Cubase project reopens showing the same name and modified state.
- **Latency:** none needed (no lookahead).

## Deliverables
1. Source in this repo, with DSP unit tests (envelope follower, tempo-grid trigger timing, Restart on vs off, Reverse, HP and LP changing the output, start/end, Dream Eater Off being a bypass, Dream Eater changing the output more as it turns up and staying stable at max, state save/restore with the embedded sample, loading old states, Rate locked with the ∞ readout while Restart is off, the Water preset loading its embedded sample, a user preset saved to a temporary folder and read back with its sample and parameters intact, and the `*` appearing after a change and clearing after saving or choosing the preset again).
2. `cargo build --release` produces the DLL. Package it as `release\Haunter.vst3\Contents\x86_64-win\Haunter.vst3` with the proper bundle layout.
3. README with the controls and the install steps.
4. Commit when it builds and the tests pass.
