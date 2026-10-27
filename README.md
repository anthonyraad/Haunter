# Haunter

Haunter is a 64-bit Windows VST3 audio effect for Cubase. You load a sample, usually a kick or another one-shot. On every beat of a tempo-synced grid, the sample's volume shape ducks the track Haunter is inserted on, like sidechaining to a ghost kick. You can also blend the sample into the output, but by default you only hear the ducking.

The plugin is `release/Haunter.vst3`.

## Install in Cubase

1. Copy the whole `release\Haunter.vst3` folder to `C:\Program Files\Common Files\VST3\`. You need administrator rights for that folder.
2. In Cubase, open Studio > VST Plug-in Manager and rescan, or restart Cubase.
3. Insert Haunter on an audio, group, or FX channel. It is listed under Dynamics.
4. Choose a factory preset such as **Water** from the preset menu, drag a WAV, AIFF, or MP3 onto the plugin window, or click the waveform area to pick a file, then press play.

The plugin works in stereo or mono and adds no latency. The C runtime is linked statically, so nothing else needs installing.

## How it works

With **Restart** on (the default), Haunter plays the selected region once from Start on every line of the tempo grid while the transport plays. Grid lines are counted from the start of the project. With Restart off, the region loops continuously from the moment the transport starts. The Rate knob keeps its look but can't be turned, and its readout shows a grey ∞ until Restart is on again. Either way, nothing starts while the transport is stopped, and a pass that is already playing finishes, then stops.

The HP and LP filters work on the sample itself as it plays. An envelope follower runs on the filtered sample, and its level sets how far the track is turned down. Depth is measured against the unfiltered sample's peak over the region: with the filters off, the track ducks by exactly Depth at the sample's loudest point. Filtering part of the sample away makes the duck shallower, and changes what you hear through Blend.

**Dream Eater** adds a soft, bell-like chord that rings in the track's key, and the same envelope that drives the ducking (after Direction, HP, and LP) plays it. There is no pitch sweep: each hit swells the chord in, and it rings out after the hit.

- **The key.** Haunter listens to the track all the time, even while Dream Eater is Off. It builds a pitch-class profile from the strongest partials over the last few seconds and matches it against major and minor key profiles. A minor key counts as its relative major, since both share the same pentatonic scale. A key is only taken once it has been clearly ahead of every other key for 1.5 s. Noise, drums, and silence leave the last key in place. Until a key has been heard, Dream Eater plays in C.
- **The chord.** Three resonators play the key's major pentatonic: the root (somewhere from A3 to G#4), the fifth, and the ninth above it. Each also has a partial an octave up at half its level that rings half as long (0.3 in 1.7.4), and the ninth has one more two octaves up, at a quarter of its level and 40% of its tail, for sparkle: about 2 to 3.7 kHz depending on the key (2.5 kHz and up from C# to G#, 2 to 2.35 kHz from A to C). The root and fifth have no top partial, since theirs would sit under 1.7 kHz and add body rather than sparkle. The high-pass at 180 Hz and the 3 dB lift above 1.5 kHz lift the top partial a little, so on white noise, the brightest material, it sits 2 to 4 dB under the ninth (12 to 13.5 dB under the whole chord), and further under on real tracks. Together they span roughly 220 Hz to 3.7 kHz. The root sits in the middle, the fifth leans left and the ninth right (75%, up from 60% in 1.7.4), with their octaves on the opposite sides (50%, up from 40%) and the top partial in the middle. They are all fed from the mid of the track, so the chord stays whole in mono. Every note is a pair of resonators, one per side, drifting up to 9 cents against each other (5 in 1.7.4), slowly, at 0.08, 0.11, and 0.14 Hz for the root, fifth, and ninth, so the pitch moves no more than about 8 cents a second and it shimmers without sounding like a chorus. One side of each pair hears the track about 6 ms later than the other, rounded to whole cycles of its note plus a quarter cycle, so the two sides ring a quarter-cycle apart: the chord is wide, and nothing cancels when it is summed to mono. Which side is late alternates from note to note.
- **What rings them.** The resonators hear only the track itself, taken before the duck and delayed, so its energy arrives in the gap right after each duck. The delay is the Attack time plus 120 ms, four of the swell's 30 ms bloom times, about when the bloom peaks and the duck is more than halfway back: 121 ms at the default 1 ms Attack, up to 220 ms at 100 ms (1.7.4: Attack plus 50 ms). It is set in time, not samples, so it is the same at every sample rate, and it glides when Attack moves. There is no noise or any other added source, and nothing saturates or distorts it, so a silent track stays silent and the chord only rings notes the track gives it something to ring.
- **The swell.** Each hit opens the chord's (delayed) input from 10% to full. The hit's envelope is held with an instant attack and a 140 ms fall, then smoothed by two 30 ms stages, so the input blooms open in a soft S-curve: a 10-90% rise of about 60 to 70 ms, peaking about 120 ms after the hit, just after the duck and just as the delayed track arrives, then halfway down about 170 ms after the peak. In 1.7.4 it rose in 25 ms, mostly hidden inside the duck. So the chord swells on the hit and rings out between hits. Turning the knob up makes the tail longer, from 0.3 s to 0.9 s, and the chord louder.
- **The tone.** The chord goes through a 24 dB/oct high-pass at 180 Hz and a gentle 3 dB lift above 1.5 kHz, so it never gets muddy or shrill.
- **A key change.** When the key changes, the new chord takes over the input over 0.4 s while the old chord rings out, so nothing clicks. The chord retunes at most once every 2 s.
- **Loudness.** The chord is added after the duck, so it blooms in the space each hit opens. At 100% it sits about 4 dB under the ducked track, judged over about a second and a half, and the track is turned down by about 2.4 dB to make room (1.6 had the chord about 7 dB under and took about 1 dB from the track). On real material that makes the output differ from Off by about 2.5 to 5 dB at 100%, against about 6 to 8 dB under in 1.6.3. The resonators are only a few hertz wide, so on real material (pads, textures, strings, drums) they keep 25 to 45 dB less power than the track; the leveller can turn them up by as much as 60 dB to reach that level, which is what makes the chord audible on any material, not only on notes right on its pitches. It turns the chord down at once but up by no more than 60 dB a second, so when the track starts, or comes back after a gap, the chord blooms in instead of bursting out. Because the chord rings the track's own notes it can line up with the track, and where the track has little at the chord's pitches the chord stays quiet, so the output is then brought back to the ducked track's loudness, turned down by up to 3 dB or up by no more than the 2.4 dB taken from the track. It aims a touch under the ducked track as the chord comes up, 0.3 dB at 100%, because on hits with gaps between them (a ride, a strummed guitar) the chord rings on through the gaps. The output stays within about 1 dB of Off. The chord is also turned down whenever it would push the output past the track's own peak over the last second or so (before the duck), the ducked track itself, or 0 dBFS.

Fully counter-clockwise reads Off, and the track is untouched, bit for bit. Dream Eater listens to the track before the ducking, and the blended sample doesn't go through it.

**Choosing the key yourself.** Right-click the Dream Eater knob for a menu with **Auto** (the default) and the twelve keys, each shown by its root alone: C, C#, D, D#, E, F, F#, G, G#, A, A#, B. A key also covers its relative minor (C covers A minor), since the chord is the pentatonic both share. Nothing on the panel shows the choice. It is saved in the project and in user presets. Factory presets and `--` set it back to Auto, and projects and presets from before 1.6 load on Auto. Right-clicking doesn't turn the knob, and changing the key doesn't add a `*` to the preset name.

The output is always at unity gain. There is no output level control.

## Controls

| Control | What it does |
| --- | --- |
| Preset menu | **--** (no sample, default settings), the factory presets **Gaze**, **Paper**, **Press**, **Spin**, **Stone**, **Umber**, **Water**, and **Wind**, then your saved presets. A `*` after the name means something has changed since the preset was loaded or saved. |
| Save | Asks for a name in place of the preset controls. Type it and press Enter or OK. Escape or Cancel closes the prompt without saving. |
| Waveform | Shows the loaded sample, with its name and length under the title. Drag the **S** and **E** handles to set the region. The orange line is the playhead. To load a sample, drop a WAV, AIFF, or MP3 file anywhere on the window, or click the waveform area to pick one. While it is empty, it reads "Drop a sample here" and a click anywhere in it opens the file picker. Once a sample is loaded, a click that isn't on a handle and isn't a drag opens it. The pointer turns into a hand where a click opens the picker, and into a sideways arrow over a handle. |
| Restart | On by default. On: the region plays once from Start on every Rate step. Off: the region loops continuously while the transport plays. Rate then reads ∞ and ignores dragging and double-clicks. |
| Direction | A menu next to Restart. **Forward** (the default) plays the region from S to E. **Backward** plays it from E to S. **Levitate** bounces between S and E: forward to E, back to S, forward again, and so on. Choosing Forward or Backward while the region plays turns the playhead round at once, from where it is, with a 1 ms crossfade, rather than waiting for the next pass. Levitate turns round at each handle without a fade: the next frame is the neighbour of the last one, so there is no jump. With Restart on, every Rate step starts it again from S going forward, and it keeps bouncing until the next step; with Restart off it bounces for as long as the transport plays. Choosing Levitate keeps the current direction until the next handle. When the transport stops, the pass in progress finishes and stops. The ducking, Dream Eater, the blended sample, and the playhead on the waveform all follow the direction actually playing. |
| Gain reduction | Shows how far the track is turned down by the ducking, in dB. |
| Rate | Grid spacing: 1/1, 1/2, 1/4, 1/8, 1/16, each also dotted or triplet. The default is 1/4. Only used while Restart is on; with Restart off it reads ∞. |
| Depth | 0–100%. How far the track ducks at the sample's peak. The default is 75%. |
| Attack | 0.1–100 ms. How quickly the follower rises. Longer values soften the start of the duck. |
| Release | 5–1000 ms. How quickly the follower falls. Longer values make the track recover more slowly. |
| Mix | Dry/wet for the ducking. At 50%, the track ducks half as far. |
| HP | High-pass on the sample, 20 Hz (Off) to 2 kHz. Affects both the ducking and the blended sample. Raise it on a kick to make the duck shallower. |
| LP | Low-pass on the sample, 50 Hz to 20 kHz (Off). Affects both the ducking and the blended sample. |
| Sample Vol | The sample's level, −48 to +12 dB. The default is 0.0 dB. |
| Blend | How much of the sample is added to the output. The default is 0%, so you only hear the duck. |
| Dream Eater | Off (the default) to 100%, in 1% steps. How loud and how long the chord rings in the track's key. The dark knob shows the sample's live swell as a purple fill inside its white arc. Right-click it to choose the key. |

Start and End are automatable parameters too. They show times in milliseconds, measured within the loaded sample. Drag a knob with the left mouse button to turn it. Double-click a knob to reset it. Hold Shift while dragging for fine adjustment.

Projects saved before 1.7 had a Reverse toggle instead of Direction. They load with Reverse on as Backward and Reverse off as Forward, and so do user presets saved before 1.7 and the preset a project had selected, so they don't show a `*`. Direction is a new parameter (ID `direction`; Reverse was `reverse`), so automation recorded on Reverse doesn't carry over. Projects saved with version 1.0 load with One-shot mode as Restart on and Loop mode as Restart off. Invert no longer exists, so it is dropped when an old project loads. Projects saved with 1.1 or earlier load with Dream Eater Off. Their Output setting is ignored, so a project that used Output to change the level plays at unity gain.

## Presets

The preset menu sits left of **Save**, at the right of the title. The preset and Direction menus show their text centred, on the button and in the list.

- **--** is the empty preset: no sample, every setting at its default. It is first in the menu, and a new Haunter starts on it. Load a sample from there as usual. Version 1.4 called it Init; projects saved on Init (or on `--` with 1.3) reopen on `--`.
- **Gaze** (8.2 s), **Paper** (20.6 s), **Press** (8.3 s), **Spin** (16.0 s), **Stone** (8.3 s), **Umber** (10.4 s), **Water** (4.2 s), and **Wind** (5.6 s) are the factory presets, listed alphabetically. Each one's sample is built into the plugin, so they work on any machine. They use the default settings, and the sample shows under the preset's name.
- **Save** stores the loaded sample's audio and every setting (Start, End, Restart, Direction, and all the knobs) as a preset with the name you type. Saved presets are listed after the factory ones and are there in every project. Saving under an existing name replaces that preset. Names can't contain `< > : " / \ | ? *` and can't be `--`, `Init`, or a factory preset's name. A preset you saved before 1.4 under a name that is now a factory preset's still shows among your presets. When the selected preset is one of yours, the prompt starts with its name, so pressing Enter updates it.
- Choosing a preset sends each setting to Cubase the same way turning a knob does, and loads its sample. Choosing the selected preset again puts it back the way it was saved.
- The name gets a `*`, as in `Water*` or `--*`, when any setting, Start or End, Restart or Direction, or the sample differs from the preset. Choosing the preset again or saving clears it.
- The selected preset's name, and what it set, are saved in the Cubase project, so a project reopens showing the same name, with the `*` if it was changed.

User presets are kept in `%APPDATA%\Haunter\Presets`, one `<name>.json` per preset. Each file holds the settings by parameter ID and the sample in the same format as the project state. To share a preset, copy its file into the same folder on another machine. To delete one, delete its file. A save is written to a temporary file first and then moved into place, so a failed save never leaves a damaged preset behind.

While you type a preset name, Haunter's window takes the keyboard. It hands the keyboard back to Cubase as soon as the name field closes, so shortcuts like the space bar work again.

## Samples and project state

- Accepted files: WAV (8, 16, 24, or 32-bit PCM, and 32 or 64-bit float, including WAVE_FORMAT_EXTENSIBLE), AIFF/AIFC (8–32-bit PCM, `sowt`, `fl32`, `fl64`), and MP3 (MPEG Layer III, mono or stereo, any bitrate). Files are recognised by their contents, so a misnamed file still loads. Only the first two channels are used. A mono file plays on both sides.
- MP3s are decoded with [Symphonia](https://github.com/pdeljanov/Symphonia), in pure Rust. When the file has a LAME or Xing header, as most do, the encoder's delay and padding are trimmed, so a kick starts right on the beat instead of about 25 ms late. A damaged frame is skipped, and a cut-off file plays what is there.
- Every format is stored the same way: Haunter keeps the decoded audio, not the original file, so an MP3 sample is saved in the project or preset exactly like a WAV.
- The sample is resampled to Cubase's sample rate with a windowed-sinc filter. The original sample is kept, so if you change the project's sample rate it is converted again from the original.
- The sample audio itself is saved in the plugin state, inside the Cubase project or preset. The project reopens with the sample even if the original file has been moved or deleted. The audio is stored as base64-encoded 32-bit float, about 470 KB per second of stereo at 44.1 kHz. Files longer than 30 seconds are cut to 30 seconds so projects stay a reasonable size.

## Build from source

You need Rust with the MSVC toolchain and Visual Studio's C++ build tools.

```powershell
cargo test --release
.\scripts\package.ps1
```

`package.ps1` runs `cargo build --release` and copies `target\release\haunter.dll` to `release\Haunter.vst3\Contents\x86_64-win\Haunter.vst3`.

The unit tests cover:

- The envelope follower's attack and release times.
- Grid timing for straight, dotted, and triplet rates, including transport start, cycle jumps, and block-size independence.
- Restart on versus off, Backward, the Start and End region, and Depth and Mix.
- Direction: choosing Backward mid-pass turns the playhead round within the next block and plays back from where it was, and Forward turns it round again; turning round adds no jump on a ramp and, on a loud 3 kHz tone, no step bigger than the tone's own (within 15%) at several awkward moments; Levitate plays S to E and back again, repeating the frame at each turn without a fade, the playhead rising and falling with it, and stays within a region set by S and E; with Restart on, each step starts Levitate again from S going forward; and its peak for the ducking is judged over the way there and back without fades.
- HP and LP measurably changing both the blended sample and the ducking.
- Dream Eater:
  - Off is a bit-exact bypass, even right after it was on, and the key is still followed while it is Off.
  - Turning it up changes the track more.
  - The chord swells on the sample's hits and rings out between them, on the key's root, fifth, and ninth and not between them, with no pitch sweep.
  - A silent track stays exactly silent at 100%, with the voices opened by every hit, at 44.1, 48, and 96 kHz: there is no hiss, and the chord rings out to true silence once the track stops.
  - The resonators hear a click exactly the Attack time plus 50 ms later, for Attacks from 0.1 to 100 ms, at 44.1, 48, and 96 kHz.
  - At full, with full-scale noise and then a full-scale tone on the root, the output stays finite, peaks at 1.02 or below, has no DC, and dies away in silence.
  - Quiet tracks stay quiet, and mono tracks stay mono.
- Key detection:
  - The FFT puts a tone in its bin.
  - Synthetic A minor and D major chord sequences are heard as the C and D pentatonics, at 44.1, 48, and 96 kHz.
  - Noise and silence have no key, and a key that was heard is held through them.
  - A key change waits until the new key has been clear for long enough, and a new key waits for the old chord to ring out.
  - Retuning from C to F# crossfades without a step in level or a click.
  - Roots sit between A3 and G#4, and the key choices round-trip through their saved IDs.
- Dream Eater's shape (since 1.7.5):
  - Only the ninth has the top partial, between 1.95 and 3.8 kHz in every key and 2.4 kHz or higher from C# to G#.
  - On noise the sparkle is between 17 and 9 dB under the whole wet, at least 1 dB under the ninth, and at least 10 dB over everything above 4.2 kHz, which stays 22 dB under the wet.
  - Summed to mono, the chord keeps its power within 0.5 dB of the two sides together, never drops by 3 dB in any 100 ms, and its sides are mostly uncorrelated.
  - Each hit blooms the swell with a 10-90% rise of 60 to 120 ms, peaking 90 to 160 ms after the hit, at least 80% open when the delayed track arrives, and halfway down 120 to 260 ms after the peak.
- Dream Eater through the real VST3 wrapper (`tests\host.rs`): a fake host calls the same VST3 functions Cubase does, loads Water through the saved state, sets parameters through parameter changes, and plays plucked chord progressions in blocks that change size every call, from 32 to 2048 frames, with a playing transport at 120 bpm. At 44.1, 48, and 96 kHz, in stereo and mono, and in place, 100% changes the track by more than 0.3 RMS against Off, brings the chord's notes up by more than 4.5 dB with at least 20 times the power it adds to notes outside the key, keeps loudness within 1.5 dB and peaks at or below 1.0, and in stereo leaves the two sides' change mostly uncorrelated, loses no more than 1 dB of it when summed to mono, and brings the ninth's sparkle up (D7, 2.35 kHz) by more than 10 dB with at least 10 times the power it adds above 4.2 kHz. A D major song rings D, A, and E, not C, and its sparkle moves to E7 (2.64 kHz), again with nothing above 4.2 kHz. The chord swells on each hit with Restart on and still sounds with it off; with it on, in 5 ms steps, the change peaks 100 to 300 ms after the hit and takes at least 40 ms to rise from 10% to 90%, and turning Dream Eater up mid-song takes effect while the stretch before it stays bit-identical to Off. A silent track comes out bit-identical to Off at 100%, so Dream Eater adds no hiss. `cargo test --release --test host host_report -- --ignored --nocapture` prints the numbers.
- Direction through the real VST3 wrapper (`tests\host.rs`): it has two steps, so three values; Forward, Backward, and Levitate sent as 0, 0.5, and 1 (and slightly off those) come back from the wrapper as exactly those values with those names and stay over the next blocks; and projects with Reverse on or off, or with a saved Direction, load as the right one.
- Direction's normalized values: 0, 0.5, and 1, the same both ways for the editor and the wrapper.
- Dream Eater on real material through the real VST3 wrapper: the factory Paper, Stone, and Wind WAVs and two synthesized legato cello lines (saw-like with a body resonance, 5 Hz vibrato, slow attacks, legato note changes, and a little bow noise in the test signal only; one in D minor, one low, down to G1), looped, peak-normalised to -6 dBFS, and played at 44.1 and 48 kHz in 512-frame blocks with Water ducking at 120 bpm. At 100% the difference from Off is between 6 and 2 dB under the track by waveform, and louder than 8 dB under by spectrum (the mean change of each FFT bin's level), the chord's notes come up by more than 4 dB and by more than 4 dB over the other notes between A3 and G#5, loudness stays within 1.25 dB of Off, peaks stay at or below 1.0, the two sides' change is mostly uncorrelated, and summing it to mono loses less than 1 dB. The cellos also pass in mono. The report also shows how much the ninth's sparkle two octaves up comes up and the mono sum. `cargo test --release --test host real_material_report -- --ignored --nocapture` prints the numbers at 10%, 50%, and 100%, and in mono, and `HAUNTER_REAL_TRACKS` adds more files (paths separated by `;`).
- `cargo test --release dream_eater_report -- --ignored --nocapture` prints how much each setting changes three materials against Off: the test track, noise, and A minor chords with hiss. For each it shows the change, loudness, peak, top end, the detected key, and how far the wet lines up with the track.
- WAV and AIFF decoding and resampling.
- MP3 decoding of `tests\fixtures\tone-1khz.mp3`, 0.25 s of a 1 kHz tone encoded by LAME: the rate, both channels at their levels, the pitch, and the exact length with the encoder delay trimmed. MP3s are recognised by their contents, a cut-off one decodes what is there, and an MP3 loaded from disk is embedded in the state and restored bit for bit after the file is deleted.
- Saving and restoring the state with the embedded sample, the selected preset, and the Dream Eater key, default values, loading 1.0 and 1.1 states, states from before 1.6 loading on Auto, Reverse on and off from before 1.7 loading as Backward and Forward (and a state with neither as Forward), and projects saved on Water or with no preset (`--` in 1.3, Init in 1.4) reopening on Water and `--`, unchanged.
- Presets: the factory presets are in alphabetical order and each decodes its built-in sample. Saving a user preset to a temporary folder and reading it back returns the same sample, bit for bit, every parameter, including values away from the defaults, and the Dream Eater key. Presets from before 1.6 load on Auto, and presets and a selected preset from before 1.7 load Reverse as Direction. Saving under an existing name replaces it. Bad and reserved names are refused, and missing or damaged files report an error.
- Headless editor checks that feed raw pointer and key events through the real editor, the way egui-baseview does, at 1x and 1.5x scale. The fake host never applies edits, like nih-plug's VST3 wrapper while the host is processing. They check that:
  - everything fits the window, with the preset menu and then Save at the right of the title, and no Load button;
  - with no sample, nothing sits between the title and the waveform, which reads "Drop a sample here", and the sample's name appears under the title once one is loaded;
  - a click anywhere on the empty waveform asks for the file picker once, with a hand pointer, but not while a file is loading or the picker is open;
  - with a sample loaded, a plain click on the waveform asks for the picker, while clicks on the S and E handles or their tabs, and drags, don't, and a drag moves Start;
  - the click that closes the preset menu over the waveform doesn't open the picker;
  - the preset and Direction menus centre their text on the button and in their lists, at 1x and 1.5x;
  - one click flips Restart, shows it, and sends a begin, set, and end to the host;
  - the Direction menu lists Forward, Backward, and Levitate, shows Forward at first, holds the edit open for a choice and sends it again every frame until the parameter has it (or about a second has passed), like a toggle held down, never sends any other value, shows the choice while the edit is open and until the host has had blocks to apply it, and explains the choices in its tooltip;
  - a choice reaches a host that, like Cubase, only passes an editor edit on to `process` while the edit is still open (an edit opened and closed in one frame never arrived, which was 1.7.0's bug), and choosing what the parameter already has closes the edit after a click's length;
  - with Restart off, Rate keeps its face, reads a grey ∞ (and egui's font has that glyph), and ignores drags, double-clicks, and the scroll wheel, and with Restart on again it reads 1/4 and turns;
  - every knob turns when dragged, and Dream Eater sits in Output's old place with its dark face, reads Off, and turns;
  - right-clicking Dream Eater opens a menu with Auto and the twelve keys, and choosing one sets the key without sending any edit or turning the knob. The panel's text is the same before and after, and a right-drag turns nothing;
  - a new instance shows `--`, and the menu lists `--`, Gaze, Paper, Press, Spin, Stone, Umber, Water, Wind, then saved presets;
  - choosing Water from the menu loads its built-in sample, sends every parameter at its default, and the audio thread plays it, `--` clears it, and loading a sample then shows `--*`;
  - choosing each factory preset loads its own built-in sample;
  - choosing a saved preset sends its values, loads its sample, and sets its Dream Eater key, factory presets and `--` set the key back to Auto, and a preset saved after the menu was last opened is shown in it;
  - the name gets a `*` after the sample, a setting, Start, Restart, or Direction changes, and loses it after choosing the preset again or saving through the prompt (Enter, OK, and Escape), and factory names are refused there.

`cargo run --release --features standalone --example standalone -- --backend dummy` opens the editor without a host. Set `HAUNTER_PREVIEW_SAMPLE` to a WAV, AIFF, or MP3 path to load it at startup.

Upstream egui-baseview ignores file drops, and on Windows its text fields get no keys inside a host. `vendor/egui-baseview` is a copy of the revision nih-plug uses, patched to pass dropped files to egui and to take keyboard focus only while a text field is being edited. See `vendor/egui-baseview/PATCHED.md`.
