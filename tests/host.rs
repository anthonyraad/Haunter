//! Drives Haunter through nih-plug's VST3 wrapper the way a host like Cubase does: the component
//! is initialised, a project with a sample in it is restored through an `IBStream`, the bus
//! arrangement and process setup are set, it is activated, and audio is processed in blocks of
//! changing size with a playing transport, with parameter values arriving in the process call's
//! `IParameterChanges`.

use haunter::preset::Preset;
use haunter::Haunter;
use nih_plug::wrapper::state::PluginState;
use nih_plug::wrapper::vst3::vst3_sys;
use nih_plug::wrapper::vst3::Wrapper;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::f64::consts::TAU;
use std::ffi::c_void;
use vst3_sys::base::{kIBSeekCur, kIBSeekEnd, kIBSeekSet, kInvalidArgument, kResultFalse, kResultOk, tresult};
use vst3_sys::base::{IBStream, IPluginBase};
use vst3_sys::utils::{SharedVstPtr, StaticVstPtr};
use vst3_sys::vst::{kMono, kStereo, AudioBusBuffers, IAudioProcessor, IComponent, IEditController};
use vst3_sys::vst::{IParamValueQueue, IParameterChanges, ParameterInfo, ProcessContext, ProcessData, ProcessSetup};
use vst3_sys::VST3;

// The `VST3` attribute expands to paths under `vst3_com`.
use vst3_sys as vst3_com;

/// A host's read-only stream over a saved project.
#[VST3(implements(IBStream))]
struct Stream {
    bytes: Vec<u8>,
    position: RefCell<usize>,
}

impl IBStream for Stream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: i32, num_bytes_read: *mut i32) -> tresult {
        let mut position = self.position.borrow_mut();
        let count = (num_bytes.max(0) as usize).min(self.bytes.len() - *position);
        std::ptr::copy_nonoverlapping(self.bytes[*position..].as_ptr(), buffer as *mut u8, count);
        *position += count;
        if !num_bytes_read.is_null() {
            *num_bytes_read = count as i32;
        }
        kResultOk
    }

    unsafe fn write(&self, _buffer: *const c_void, _num_bytes: i32, _num_bytes_written: *mut i32) -> tresult {
        kResultFalse
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> tresult {
        let mut position = self.position.borrow_mut();
        let base = match mode {
            m if m == kIBSeekSet => 0,
            m if m == kIBSeekCur => *position as i64,
            m if m == kIBSeekEnd => self.bytes.len() as i64,
            _ => return kInvalidArgument,
        };
        *position = (base + pos).clamp(0, self.bytes.len() as i64) as usize;
        if !result.is_null() {
            *result = *position as i64;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        *pos = *self.position.borrow() as i64;
        kResultOk
    }
}

/// One parameter's changes in a block.
#[VST3(implements(IParamValueQueue))]
struct Queue {
    id: u32,
    points: Vec<(i32, f64)>,
}

impl IParamValueQueue for Queue {
    unsafe fn get_parameter_id(&self) -> u32 {
        self.id
    }

    unsafe fn get_point_count(&self) -> i32 {
        self.points.len() as i32
    }

    unsafe fn get_point(&self, index: i32, sample_offset: *mut i32, value: *mut f64) -> tresult {
        match self.points.get(index as usize) {
            Some(&(offset, normalized)) => {
                *sample_offset = offset;
                *value = normalized;
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn add_point(&self, _sample_offset: i32, _value: f64, _index: *mut i32) -> tresult {
        kResultFalse
    }
}

fn queue_pointer(queue: Option<&Queue>) -> StaticVstPtr<dyn IParamValueQueue> {
    let pointer: *const Queue = queue.map_or(std::ptr::null(), |queue| queue);
    // `StaticVstPtr` is a transparent COM pointer, and a co-class object starts with its vtable.
    unsafe { std::mem::transmute::<*const Queue, StaticVstPtr<dyn IParamValueQueue>>(pointer) }
}

/// Every parameter change in a block.
#[VST3(implements(IParameterChanges))]
struct Changes {
    queues: Vec<Box<Queue>>,
}

impl IParameterChanges for Changes {
    unsafe fn get_parameter_count(&self) -> i32 {
        self.queues.len() as i32
    }

    unsafe fn get_parameter_data(&self, index: i32) -> StaticVstPtr<dyn IParamValueQueue> {
        queue_pointer(self.queues.get(index as usize).map(|queue| &**queue))
    }

    unsafe fn add_parameter_data(&self, _id: *const u32, _index: *mut i32) -> StaticVstPtr<dyn IParamValueQueue> {
        queue_pointer(None)
    }
}

const PLAYING: u32 = 1 << 1;
const PROJECT_TIME_MUSIC_VALID: u32 = 1 << 9;
const TEMPO_VALID: u32 = 1 << 10;
const BAR_POSITION_VALID: u32 = 1 << 11;
const TIME_SIG_VALID: u32 = 1 << 13;
const TEMPO: f64 = 120.0;
const MAX_BLOCK: usize = 2048;
/// Block sizes a host might use, cycled through so that consecutive calls differ.
const BLOCKS: [usize; 9] = [512, 32, 2048, 333, 1024, 64, 1500, 128, 997];

/// A project with `sample` loaded and every parameter at its default, as nih-plug saves it.
fn project_with(sample: &str) -> Vec<u8> {
    let sample = Preset::factory(sample).unwrap().sample.unwrap();
    let mut fields = BTreeMap::new();
    fields.insert("sample".to_owned(), serde_json::to_string(&Some(sample)).unwrap());
    let state = PluginState { version: env!("CARGO_PKG_VERSION").to_owned(), params: BTreeMap::new(), fields };
    serde_json::to_vec(&state).unwrap()
}

/// Plucked chords, given as MIDI notes, two seconds each and round and round, in stereo.
fn chords(progression: &[&[u8]], rate: f64, seconds: f64) -> Vec<Vec<f32>> {
    let frames = (rate * seconds) as usize;
    let per_chord = (rate * 2.0) as usize;
    let mut out = vec![Vec::with_capacity(frames), Vec::with_capacity(frames)];
    for n in 0..frames {
        let chord = progression[(n / per_chord) % progression.len()];
        let t = (n % per_chord) as f64 / rate;
        let pluck = (1.0 - (-t * 80.0).exp()) * (-t * 0.8).exp();
        let mut sides = [0.0f64; 2];
        for (voice, &note) in chord.iter().enumerate() {
            let hz = 440.0 * ((note as f64 - 69.0) / 12.0).exp2();
            let phase = TAU * hz * n as f64 / rate + voice as f64;
            let tone: f64 = (1..=5).map(|h| (phase * h as f64).sin() / (h * h) as f64).sum();
            let pan = voice as f64 / 3.0;
            sides[0] += tone * (1.0 - 0.4 * pan);
            sides[1] += tone * (0.6 + 0.4 * pan);
        }
        for (side, value) in out.iter_mut().zip(sides) {
            side.push((0.12 * pluck * value) as f32);
        }
    }
    out
}

/// Am F C G, whose pentatonic scale is C major's.
const A_MINOR: &[&[u8]] = &[&[45, 57, 60, 64], &[41, 57, 60, 65], &[48, 55, 60, 64], &[43, 55, 59, 62]];
/// D A Bm G.
const D_MAJOR: &[&[u8]] = &[&[38, 50, 54, 57], &[45, 57, 61, 64], &[47, 54, 59, 62], &[43, 55, 59, 62]];

struct Host {
    wrapper: Box<Wrapper<Haunter>>,
    rate: f64,
    channels: usize,
    position: usize,
    /// Parameter IDs by name.
    ids: BTreeMap<String, u32>,
    /// Normalized values to send with the next block.
    pending: Vec<(u32, f64)>,
}

impl Host {
    /// A plugin instance with `project` restored, set up and activated for `channels` at `rate`.
    fn new(rate: f64, channels: usize, project: &[u8]) -> Self {
        let wrapper = Wrapper::<Haunter>::new();
        let mut ids = BTreeMap::new();
        unsafe {
            assert_eq!(IPluginBase::initialize(&*wrapper, std::ptr::null_mut()), kResultOk);
            for index in 0..IEditController::get_parameter_count(&*wrapper) {
                let mut info: ParameterInfo = std::mem::zeroed();
                assert_eq!(IEditController::get_parameter_info(&*wrapper, index, &mut info), kResultOk);
                let title = String::from_utf16_lossy(&info.title.iter().take_while(|&&c| c != 0).map(|&c| c as u16).collect::<Vec<_>>());
                ids.insert(title, info.id);
            }
            let stream = Stream::allocate(project.to_vec(), RefCell::new(0));
            let pointer = std::mem::transmute::<*const Stream, SharedVstPtr<dyn IBStream>>(&*stream);
            assert_eq!(IComponent::set_state(&*wrapper, pointer), kResultOk, "the project loads");
            let mut inputs = [if channels == 1 { kMono } else { kStereo }];
            let mut outputs = inputs;
            assert_eq!(wrapper.set_bus_arrangements(inputs.as_mut_ptr(), 1, outputs.as_mut_ptr(), 1), kResultOk);
            let setup = ProcessSetup {
                process_mode: 0,
                symbolic_sample_size: 0,
                max_samples_per_block: MAX_BLOCK as i32,
                sample_rate: rate,
            };
            assert_eq!(wrapper.setup_processing(&setup), kResultOk);
            assert_eq!(IComponent::set_active(&*wrapper, 1), kResultOk);
            assert_eq!(wrapper.set_processing(1), kResultOk);
        }
        Host { wrapper, rate, channels, position: 0, ids, pending: Vec::new() }
    }

    /// Sends `name` the normalized `value` at the start of the next block, as a knob or
    /// automation edit does.
    fn set(&mut self, name: &str, value: f64) {
        let id = *self.ids.get(name).unwrap_or_else(|| panic!("no parameter called {name}"));
        self.pending.push((id, value));
    }

    /// Processes `audio` in place, in blocks of changing size, through separate input and
    /// output buffers unless `in_place`.
    fn process(&mut self, audio: &mut [Vec<f32>], in_place: bool) {
        self.process_in(audio, in_place, 0);
    }

    /// As `process`, in blocks of `block` frames, or of changing size if it is 0.
    fn process_in(&mut self, audio: &mut [Vec<f32>], in_place: bool, block_size: usize) {
        let frames = audio[0].len();
        let (mut done, mut block) = (0, 0);
        while done < frames {
            let size = if block_size > 0 { block_size } else { BLOCKS[block % BLOCKS.len()] };
            let count = size.min(frames - done);
            self.process_block(audio, done, count, in_place);
            done += count;
            block += 1;
        }
    }

    fn process_block(&mut self, audio: &mut [Vec<f32>], start: usize, count: usize, in_place: bool) {
        let mut outputs_copy: Vec<Vec<f32>> = vec![vec![0.0; count]; audio.len()];
        let mut input_pointers: Vec<*mut c_void> =
            audio.iter_mut().map(|side| side[start..].as_mut_ptr() as *mut c_void).collect();
        let mut output_pointers: Vec<*mut c_void> = if in_place {
            input_pointers.clone()
        } else {
            outputs_copy.iter_mut().map(|side| side.as_mut_ptr() as *mut c_void).collect()
        };
        let mut inputs = AudioBusBuffers {
            num_channels: self.channels as i32,
            silence_flags: 0,
            buffers: input_pointers.as_mut_ptr(),
        };
        let mut outputs = AudioBusBuffers {
            num_channels: self.channels as i32,
            silence_flags: 0,
            buffers: output_pointers.as_mut_ptr(),
        };
        let beat = self.position as f64 / self.rate * TEMPO / 60.0;
        let mut context = ProcessContext {
            state: PLAYING | PROJECT_TIME_MUSIC_VALID | TEMPO_VALID | BAR_POSITION_VALID | TIME_SIG_VALID,
            sample_rate: self.rate,
            project_time_samples: self.position as i64,
            continuous_time_samples: self.position as i64,
            project_time_music: beat,
            bar_position_music: (beat / 4.0).floor() * 4.0,
            tempo: TEMPO,
            time_sig_num: 4,
            time_sig_den: 4,
            ..Default::default()
        };
        let queues = self.pending.drain(..).map(|(id, value)| Queue::allocate(id, vec![(0, value)])).collect();
        let changes = Changes::allocate(queues);
        unsafe {
            let mut data = ProcessData {
                process_mode: 0,
                symbolic_sample_size: 0,
                num_samples: count as i32,
                num_inputs: 1,
                num_outputs: 1,
                inputs: &mut inputs,
                outputs: &mut outputs,
                input_param_changes: std::mem::transmute::<*const Changes, StaticVstPtr<dyn IParameterChanges>>(&*changes),
                output_param_changes: std::mem::zeroed(),
                input_events: std::mem::zeroed(),
                output_events: std::mem::zeroed(),
                context: &mut context,
            };
            assert_eq!(self.wrapper.process(&mut data), kResultOk);
        }
        if !in_place {
            for (side, output) in audio.iter_mut().zip(outputs_copy) {
                side[start..start + count].copy_from_slice(&output);
            }
        }
        self.position += count;
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            self.wrapper.set_processing(0);
            IComponent::set_active(&*self.wrapper, 0);
            IPluginBase::terminate(&*self.wrapper);
        }
    }
}

fn rms(signal: &[f32]) -> f64 {
    (signal.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / signal.len().max(1) as f64).sqrt()
}

/// Mean power near `hz`, from Goertzel filters over quarter-second windows, which hear a band of
/// a few hertz around it and ignore phase.
fn band_power(signal: &[f32], hz: f64, rate: f64) -> f64 {
    let window = (rate * 0.25) as usize;
    let coefficient = 2.0 * (TAU * hz / rate).cos();
    let windows: Vec<f64> = signal
        .chunks_exact(window)
        .map(|chunk| {
            let (mut s1, mut s2) = (0.0, 0.0);
            for (n, &v) in chunk.iter().enumerate() {
                let hann = 0.5 - 0.5 * (TAU * n as f64 / window as f64).cos();
                let s0 = v as f64 * hann + coefficient * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            s1 * s1 + s2 * s2 - coefficient * s1 * s2
        })
        .collect();
    windows.iter().sum::<f64>() / windows.len() as f64
}

fn midi_hz(note: f64) -> f64 {
    440.0 * ((note - 69.0) / 12.0).exp2()
}

/// How many dB louder the band around each MIDI note is in `on` than in `off`, averaged.
fn band_gain_db(on: &[f32], off: &[f32], notes: &[f64], rate: f64) -> f64 {
    let gains: Vec<f64> = notes
        .iter()
        .map(|&note| 10.0 * (band_power(on, midi_hz(note), rate) / band_power(off, midi_hz(note), rate)).log10())
        .collect();
    gains.iter().sum::<f64>() / gains.len() as f64
}

/// How much power `on` adds to `off` near each MIDI note, averaged.
fn added_power(on: &[f32], off: &[f32], notes: &[f64], rate: f64) -> f64 {
    let added: f64 = notes.iter().map(|&note| band_power(on, midi_hz(note), rate) - band_power(off, midi_hz(note), rate)).sum();
    added / notes.len() as f64
}

struct Settings {
    rate: f64,
    channels: usize,
    dream_eater: f64,
    restart: bool,
    in_place: bool,
    /// Frames per block, or 0 for blocks of changing size.
    block: usize,
}

impl Settings {
    fn at(rate: f64, channels: usize, dream_eater: f64) -> Self {
        Settings { rate, channels, dream_eater, restart: true, in_place: false, block: 0 }
    }
}

/// Renders `music` through a fresh instance with Water loaded, Dream Eater sent with the first
/// block, and returns each channel from `skip` seconds on.
fn render(music: &[Vec<f32>], settings: &Settings, skip: f64) -> Vec<Vec<f32>> {
    let mut host = Host::new(settings.rate, settings.channels, &project_with("Water"));
    host.set("Dream Eater", settings.dream_eater);
    host.set("Restart", if settings.restart { 1.0 } else { 0.0 });
    let mut audio = if settings.channels == 1 {
        vec![music[0].iter().zip(&music[1]).map(|(l, r)| 0.5 * (l + r)).collect()]
    } else {
        music.to_vec()
    };
    host.process_in(&mut audio, settings.in_place, settings.block);
    let from = (skip * settings.rate) as usize;
    audio.into_iter().map(|side| side[from..].to_vec()).collect()
}

/// What turning Dream Eater from Off to `settings.dream_eater` does to 12 seconds of `music`,
/// over the last 6.
struct Change {
    /// RMS of the difference, relative to Off.
    difference: f64,
    loudness_db: f64,
    peak: f32,
    /// Off and on, every channel end to end.
    off: Vec<f32>,
    on: Vec<f32>,
    /// The difference on the left and right, for stereo.
    sides: Vec<Vec<f32>>,
}

fn change(progression: &[&[u8]], settings: Settings) -> Change {
    let music = chords(progression, settings.rate, 12.0);
    let off = render(&music, &Settings { dream_eater: 0.0, ..settings }, 6.0);
    let on = render(&music, &settings, 6.0);
    let sides: Vec<Vec<f32>> = on.iter().zip(&off).map(|(a, b)| a.iter().zip(b).map(|(a, b)| a - b).collect()).collect();
    let (off, on) = (off.concat(), on.concat());
    Change {
        difference: rms(&sides.concat()) / rms(&off),
        loudness_db: 20.0 * (rms(&on) / rms(&off)).log10(),
        peak: on.iter().fold(0.0f32, |m, v| m.max(v.abs())),
        off,
        on,
        sides,
    }
}

fn correlation(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(&a, &b)| a as f64 * b as f64).sum();
    dot / (rms(a) * rms(b) * a.len() as f64).max(1e-30)
}

/// The C major pentatonic chord Dream Eater rings for A minor: C4, G4, D5.
const C_CHORD: [f64; 3] = [60.0, 67.0, 74.0];
/// The D major one: D4, A4, E5.
const D_CHORD: [f64; 3] = [62.0, 69.0, 76.0];
/// Notes outside C major, which the test songs never play.
const OUTSIDE_C_MAJOR: [f64; 6] = [61.0, 63.0, 66.0, 68.0, 70.0, 73.0];

#[test]
fn dream_eater_is_clearly_audible_through_the_vst3_wrapper() {
    for rate in [44_100.0, 48_000.0, 96_000.0] {
        for channels in [2, 1] {
            let label = format!("{rate} Hz, {channels} channel(s)");
            let result = change(A_MINOR, Settings { in_place: rate == 48_000.0, ..Settings::at(rate, channels, 1.0) });
            assert!(result.difference > 0.3, "{label}: the output changes by {:.3} of Off", result.difference);
            let chord = band_gain_db(&result.on, &result.off, &C_CHORD, rate);
            assert!(chord > 4.5, "{label}: the chord's notes come up {chord:.1} dB");
            let in_chord = added_power(&result.on, &result.off, &C_CHORD, rate);
            let outside = added_power(&result.on, &result.off, &OUTSIDE_C_MAJOR, rate);
            assert!(in_chord > 20.0 * outside.max(0.0), "{label}: the chord's notes, not others: {in_chord:e} vs {outside:e}");
            assert!(result.loudness_db.abs() < 1.5, "{label}: about as loud as Off: {:+.2} dB", result.loudness_db);
            assert!(result.peak <= 1.0, "{label}: peak {}", result.peak);
            if channels == 2 {
                let spread = correlation(&result.sides[0], &result.sides[1]);
                assert!(spread < 0.8, "{label}: the chord shimmers across the stereo field: {spread:.2}");
            }
        }
    }
}

#[test]
fn dream_eater_rings_the_detected_key_through_the_vst3_wrapper() {
    let result = change(D_MAJOR, Settings::at(48_000.0, 2, 1.0));
    let d = added_power(&result.on, &result.off, &D_CHORD, 48_000.0);
    // The C chord Dream Eater starts on, before it has heard the key.
    let c = added_power(&result.on, &result.off, &C_CHORD, 48_000.0);
    assert!(d > 20.0 * c.max(0.0), "D major rings D, A, E: {d:e}, not C, G, D: {c:e}");
}

#[test]
fn dream_eater_swells_on_each_hit_with_restart_on_and_off() {
    for restart in [true, false] {
        let result = change(A_MINOR, Settings { restart, ..Settings::at(48_000.0, 2, 1.0) });
        assert!(result.difference > 0.3, "Restart {restart}: {:.3}", result.difference);
        if !restart {
            continue;
        }
        // The 50 ms RMS of the change across each beat, averaged over the beats.
        let beat = 24_000;
        let windows = beat / 2_400;
        let mut shape = vec![0.0; windows];
        let left = &result.sides[0];
        for start in (0..left.len() - beat).step_by(beat) {
            for (window, level) in shape.iter_mut().enumerate() {
                *level += rms(&left[start + window * 2_400..start + (window + 1) * 2_400]);
            }
        }
        let (loudest, quietest) = (shape.iter().cloned().fold(0.0, f64::max), shape.iter().cloned().fold(f64::MAX, f64::min));
        assert!(loudest > 2.0 * quietest, "the chord swells and fades with each hit: {shape:?}");
    }
}

#[test]
fn turning_dream_eater_up_mid_song_takes_effect_and_off_is_untouched() {
    let rate = 48_000.0;
    let music = chords(A_MINOR, rate, 8.0);
    let mut off = music.clone();
    Host::new(rate, 2, &project_with("Water")).process(&mut off, false);

    let mut host = Host::new(rate, 2, &project_with("Water"));
    let mut turned = music.clone();
    let split = 4 * rate as usize;
    let (mut first, mut second): (Vec<Vec<f32>>, Vec<Vec<f32>>) =
        turned.iter_mut().map(|side| (side[..split].to_vec(), side[split..].to_vec())).unzip();
    host.process(&mut first, true);
    host.set("Dream Eater", 1.0);
    host.process(&mut second, true);
    for (side, (first, second)) in first.iter().zip(&second).enumerate() {
        assert_eq!(first[..], off[side][..split], "Off leaves the plugin's output as it was");
        let difference: Vec<f32> = second.iter().zip(&off[side][split..]).map(|(a, b)| a - b).collect();
        let from = rate as usize;
        let change = rms(&difference[from..]) / rms(&off[side][split + from..]);
        // About -11 dB under the track by waveform, while the chord rings in the gaps the duck opens.
        assert!(change > 0.25, "side {side}: {change:.3}");
    }
}

/// The loudest difference Dream Eater at 100% makes to `seconds` of a silent track, which a
/// noise source would fill with hiss.
fn change_on_silence(rate: f64, channels: usize, seconds: f64) -> f32 {
    let silence = vec![vec![0.0f32; (rate * seconds) as usize]; 2];
    let off = render(&silence, &Settings::at(rate, channels, 0.0), 0.0);
    let on = render(&silence, &Settings::at(rate, channels, 1.0), 0.0);
    on.iter().zip(&off).flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (a - b).abs())).fold(0.0, f32::max)
}

#[test]
fn dream_eater_adds_no_hiss_to_silence_through_the_vst3_wrapper() {
    for rate in [44_100.0, 48_000.0, 96_000.0] {
        for channels in [2, 1] {
            let change = change_on_silence(rate, channels, 4.0);
            assert_eq!(change, 0.0, "{rate} Hz, {channels} channel(s): a silent track stays as it is at Off");
        }
    }
}

#[test]
#[ignore]
fn host_report() {
    for rate in [44_100.0, 48_000.0, 96_000.0] {
        for channels in [2, 1] {
            for amount in [0.1, 0.5, 1.0] {
                let result = change(A_MINOR, Settings::at(rate, channels, amount));
                let spread = if channels == 2 { correlation(&result.sides[0], &result.sides[1]) } else { 1.0 };
                println!(
                    "{rate} Hz, {channels} ch, {amount}: diff {:.3}, loudness {:+.2} dB, peak {:.3}, chord {:+.1} dB, \
                     other notes {:+.1} dB, added in chord / outside {:.0}, L/R {spread:.2}",
                    result.difference,
                    result.loudness_db,
                    result.peak,
                    band_gain_db(&result.on, &result.off, &C_CHORD, rate),
                    band_gain_db(&result.on, &result.off, &OUTSIDE_C_MAJOR, rate),
                    added_power(&result.on, &result.off, &C_CHORD, rate)
                        / added_power(&result.on, &result.off, &OUTSIDE_C_MAJOR, rate),
                );
            }
        }
    }
    for restart in [true, false] {
        let result = change(A_MINOR, Settings { restart, ..Settings::at(48_000.0, 2, 1.0) });
        println!("Restart {restart}: diff {:.3}", result.difference);
    }
    for rate in [44_100.0, 48_000.0] {
        println!("{rate} Hz, silence at 100%: largest change {:e}", change_on_silence(rate, 2, 4.0));
    }
    let result = change(D_MAJOR, Settings::at(48_000.0, 2, 1.0));
    println!(
        "D major: power added at the D chord {:e}, at the C chord {:e}",
        added_power(&result.on, &result.off, &D_CHORD, 48_000.0),
        added_power(&result.on, &result.off, &C_CHORD, 48_000.0)
    );
}

/// A real recording as a stereo track at `rate`: decoded, resampled, looped to `seconds`, and
/// peak-normalised to -6 dBFS, as a track would arrive from a mixer channel.
fn real_track(path: &std::path::Path, rate: f64, seconds: f64) -> Vec<Vec<f32>> {
    let audio = haunter::audio_file::decode_file(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let mut sides: Vec<Vec<f32>> = (0..2)
        .map(|side| {
            let channel = &audio.channels[side.min(audio.channels.len() - 1)];
            haunter::sample::resample(channel, audio.sample_rate, rate)
        })
        .collect();
    let frames = (rate * seconds) as usize;
    for side in &mut sides {
        let source = side.clone();
        *side = (0..frames).map(|n| source[n % source.len()]).collect();
    }
    let peak = sides.iter().flatten().fold(0.0f32, |m, v| m.max(v.abs())).max(1e-9);
    for side in &mut sides {
        side.iter_mut().for_each(|v| *v *= 0.5 / peak);
    }
    sides
}

/// A factory sample in `presets\`, as a track.
fn factory_track(name: &str, rate: f64, seconds: f64) -> Vec<Vec<f32>> {
    real_track(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("presets").join(format!("{name}.wav")), rate, seconds)
}

/// In-place radix-2 FFT.
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut length = 2;
    while length <= n {
        let step = -TAU / length as f64;
        for start in (0..n).step_by(length) {
            for k in 0..length / 2 {
                let (wr, wi) = ((step * k as f64).cos(), (step * k as f64).sin());
                let (a, b) = (start + k, start + k + length / 2);
                let tr = re[b] * wr - im[b] * wi;
                let ti = re[b] * wi + im[b] * wr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        length <<= 1;
    }
}

/// Magnitude spectra of `signal` over Hann windows of 4096 frames, hopping by half a window.
fn spectra(signal: &[f32]) -> Vec<Vec<f64>> {
    const SIZE: usize = 4_096;
    (0..signal.len().saturating_sub(SIZE))
        .step_by(SIZE / 2)
        .map(|start| {
            let mut re: Vec<f64> = (0..SIZE)
                .map(|n| signal[start + n] as f64 * (0.5 - 0.5 * (TAU * n as f64 / SIZE as f64).cos()))
                .collect();
            let mut im = vec![0.0; SIZE];
            fft(&mut re, &mut im);
            (0..SIZE / 2).map(|k| re[k].hypot(im[k])).collect()
        })
        .collect()
}

/// How much the magnitude spectrum changes, in dB relative to Off's, frame by frame: what the
/// ear hears change, unlike a sample-by-sample difference, which also counts a phase shift.
fn spectral_change_db(on: &[f32], off: &[f32]) -> f64 {
    let (on, off) = (spectra(on), spectra(off));
    let (mut change, mut total) = (0.0, 0.0);
    for (a, b) in on.iter().zip(&off) {
        for (a, b) in a.iter().zip(b) {
            change += (a - b).powi(2);
            total += b * b;
        }
    }
    10.0 * (change / total.max(1e-30)).log10()
}

/// The MIDI notes Dream Eater's chord rings in `key`: root, fifth, and ninth.
fn chord_notes(key: u8) -> [f64; 3] {
    let root = 57.0 + ((key as usize + 3) % 12) as f64;
    [root, root + 7.0, root + 14.0]
}

/// What Dream Eater at `settings.dream_eater` does to a real `track`, over its last 6 seconds.
struct RealChange {
    /// RMS of the difference from Off, in dB relative to Off.
    difference_db: f64,
    spectral_db: f64,
    loudness_db: f64,
    peak: f32,
    /// The key whose chord comes up most, and by how many dB; and how much the other notes from
    /// A3 to G#5 move.
    key: u8,
    chord_db: f64,
    others_db: f64,
    spread: f64,
}

fn real_change(track: &[Vec<f32>], settings: Settings) -> RealChange {
    let rate = settings.rate;
    let off = render(track, &Settings { dream_eater: 0.0, ..settings }, 6.0);
    let on = render(track, &settings, 6.0);
    let sides: Vec<Vec<f32>> = on.iter().zip(&off).map(|(a, b)| a.iter().zip(b).map(|(a, b)| a - b).collect()).collect();
    let spread = if sides.len() > 1 { correlation(&sides[0], &sides[1]) } else { 1.0 };
    let spectral = off.iter().zip(&on).map(|(off, on)| 10f64.powf(spectral_change_db(on, off) / 10.0)).sum::<f64>() / off.len() as f64;
    let (off, on) = (off.concat(), on.concat());
    let (key, chord_db) = (0..12u8)
        .map(|key| (key, band_gain_db(&on, &off, &chord_notes(key), rate)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap();
    let chord = chord_notes(key);
    let others: Vec<f64> = (57..81).map(|n| n as f64).filter(|n| !chord.iter().any(|c| (c - n).rem_euclid(12.0) == 0.0)).collect();
    RealChange {
        difference_db: 20.0 * (rms(&sides.concat()) / rms(&off)).log10(),
        spectral_db: 10.0 * spectral.log10(),
        loudness_db: 20.0 * (rms(&on) / rms(&off)).log10(),
        peak: on.iter().fold(0.0f32, |m, v| m.max(v.abs())),
        key,
        chord_db,
        others_db: band_gain_db(&on, &off, &others, rate),
        spread,
    }
}

impl std::fmt::Display for RealChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "difference {:+.1} dB, spectrum {:+.1} dB, loudness {:+.2} dB, peak {:.3}, chord of {} {:+.1} dB, other notes {:+.1} dB, L/R {:.2}",
            self.difference_db,
            self.spectral_db,
            self.loudness_db,
            self.peak,
            haunter::dream::KEY_NAMES[self.key as usize],
            self.chord_db,
            self.others_db,
            self.spread
        )
    }
}

/// Real recordings to report on besides the factory samples, from `HAUNTER_REAL_TRACKS`,
/// separated by semicolons.
fn extra_tracks() -> Vec<std::path::PathBuf> {
    std::env::var("HAUNTER_REAL_TRACKS")
        .map(|list| list.split(';').filter(|p| !p.trim().is_empty()).map(|p| p.trim().into()).collect())
        .unwrap_or_default()
}

/// A legato cello line, as MIDI notes `note_seconds` each, gliding between them: a bowed,
/// sawtooth-like tone with a body resonance, 5 Hz vibrato, a slow first attack, and a little bow
/// noise, the noise only in this test signal. Few transients, so only the duck can play the chord.
fn cello(notes: &[f64], rate: f64, seconds: f64, note_seconds: f64) -> Vec<Vec<f32>> {
    let frames = (rate * seconds) as usize;
    let glide = 1.0 - (-1.0 / (0.02 * rate)).exp();
    let (mut pitch, mut phase, mut seed) = (notes[0], 0.0f64, 0x1234_5678u32);
    let mut out = vec![Vec::with_capacity(frames), Vec::with_capacity(frames)];
    let lag = (0.0007 * rate) as usize;
    let mut tone = Vec::with_capacity(frames);
    for n in 0..frames {
        let t = n as f64 / rate;
        let note = notes[((t / note_seconds) as usize) % notes.len()];
        pitch += glide * (note - pitch);
        let vibrato = 0.15 * (TAU * 5.2 * t).sin() * ((t % note_seconds) / 0.3).min(1.0);
        let hz = midi_hz(pitch + vibrato);
        phase += TAU * hz / rate;
        let mut value = 0.0;
        for h in 1..40 {
            let f = hz * h as f64;
            if f > rate / 2.2 {
                break;
            }
            let body = 1.0 + 0.6 * (-((f - 600.0) / 300.0).powi(2)).exp();
            value += body / (h as f64 * (1.0 + (f / 3_000.0).powi(2))) * (h as f64 * (phase + 0.3)).sin();
        }
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        value += 0.01 * ((seed >> 8) as f64 / (1u32 << 23) as f64 - 1.0);
        tone.push(value * (0.5 + 0.5 * (t / 0.25).min(1.0)));
    }
    let peak = tone.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    for n in 0..frames {
        out[0].push((0.5 * tone[n] / peak) as f32);
        out[1].push((0.475 * tone[n.saturating_sub(lag)] / peak) as f32);
    }
    out
}

/// D minor and C minor cello lines: D3 F3 A3 G3 E3 D3 A2 C3, and the low strings in C minor.
const CELLO_D_MINOR: [f64; 8] = [50.0, 53.0, 57.0, 55.0, 52.0, 50.0, 45.0, 48.0];
const CELLO_LOW: [f64; 8] = [36.0, 43.0, 41.0, 38.0, 39.0, 36.0, 31.0, 34.0];

/// Real and realistic tracks Dream Eater must be clearly heard on, as the user hears it: two
/// factory samples, which are textures with little at the chord's notes, and legato cello.
fn audible_tracks(rate: f64) -> Vec<(&'static str, Vec<Vec<f32>>)> {
    vec![
        ("Paper", factory_track("Paper", rate, 12.0)),
        ("Stone", factory_track("Stone", rate, 12.0)),
        ("Wind", factory_track("Wind", rate, 12.0)),
        ("legato cello in D minor", cello(&CELLO_D_MINOR, rate, 12.0, 1.5)),
        ("low legato cello", cello(&CELLO_LOW, rate, 12.0, 1.5)),
    ]
}

#[test]
fn dream_eater_is_clearly_audible_on_real_tracks_through_the_vst3_wrapper() {
    for rate in [48_000.0, 44_100.0] {
        for (name, track) in audible_tracks(rate) {
            let result = real_change(&track, Settings { block: 512, ..Settings::at(rate, 2, 1.0) });
            let label = format!("{name} at {rate} Hz: {result}");
            assert!(result.spectral_db > -12.0, "{label}: the spectrum changes clearly");
            assert!(result.difference_db > -12.0, "{label}: the difference is no more than 12 dB under the track");
            assert!(result.chord_db > 4.0, "{label}: the chord's notes come up");
            assert!(result.chord_db > result.others_db + 4.0, "{label}: and more than the other notes");
            assert!(result.loudness_db.abs() < 1.5, "{label}: about as loud as Off");
            assert!(result.peak <= 1.0, "{label}");
            assert!(result.spread < 0.5, "{label}: the chord is wide");
        }
    }
}

#[test]
fn dream_eater_is_audible_on_mono_cello_through_the_vst3_wrapper() {
    let track = cello(&CELLO_D_MINOR, 48_000.0, 12.0, 1.5);
    let result = real_change(&track, Settings { block: 512, ..Settings::at(48_000.0, 1, 1.0) });
    assert!(result.difference_db > -12.0 && result.chord_db > 4.0, "mono: {result}");
    assert!(result.loudness_db.abs() < 1.5 && result.peak <= 1.0, "mono: {result}");
}

#[test]
#[ignore]
fn real_material_report() {
    let rate = 48_000.0;
    let mut tracks: Vec<(String, Vec<Vec<f32>>)> =
        ["Water", "Paper", "Stone", "Umber", "Wind"].iter().map(|name| (name.to_string(), factory_track(name, rate, 12.0))).collect();
    for path in extra_tracks() {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        tracks.push((name, real_track(&path, rate, 12.0)));
    }
    tracks.push(("legato cello in D minor".into(), cello(&CELLO_D_MINOR, rate, 12.0, 1.5)));
    tracks.push(("low legato cello".into(), cello(&CELLO_LOW, rate, 12.0, 1.5)));
    for (name, track) in &tracks {
        for amount in [0.1, 0.5, 1.0] {
            let result = real_change(track, Settings { block: 512, ..Settings::at(rate, 2, amount) });
            println!("{name} at {amount}: {result}");
        }
        let mono = real_change(track, Settings { block: 512, ..Settings::at(rate, 1, 1.0) });
        println!("{name} in mono at 1: {mono}");
    }
}
