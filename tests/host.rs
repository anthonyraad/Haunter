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

/// A project with `params` saved in it as well as Water.
fn project_with_params(params: &[(&str, nih_plug::wrapper::state::ParamValue)]) -> Vec<u8> {
    let mut state: PluginState = serde_json::from_slice(&project_with("Water")).unwrap();
    for (id, value) in params {
        state.params.insert((*id).to_owned(), value.clone());
    }
    serde_json::to_vec(&state).unwrap()
}

impl Host {
    fn normalized(&self, name: &str) -> f64 {
        unsafe { IEditController::get_param_normalized(&*self.wrapper, self.ids[name]) }
    }

    fn shown(&self, name: &str, normalized: f64) -> String {
        let mut text = [0i16; 128];
        unsafe {
            assert_eq!(IEditController::get_param_string_by_value(&*self.wrapper, self.ids[name], normalized, text.as_mut_ptr()), kResultOk);
        }
        String::from_utf16_lossy(&text.iter().take_while(|&&c| c != 0).map(|&c| c as u16).collect::<Vec<_>>())
    }
}

#[test]
fn direction_round_trips_through_the_vst3_wrapper() {
    let mut host = Host::new(48_000.0, 2, &project_with("Water"));
    let id = host.ids["Direction"];
    unsafe {
        let mut info: ParameterInfo = std::mem::zeroed();
        let index = (0..IEditController::get_parameter_count(&*host.wrapper))
            .find(|&index| {
                IEditController::get_parameter_info(&*host.wrapper, index, &mut info);
                info.id == id
            })
            .unwrap();
        assert_eq!(IEditController::get_parameter_info(&*host.wrapper, index, &mut info), kResultOk);
        assert_eq!(info.step_count, 2, "three choices");
        assert_eq!(info.default_normalized_value, 0.0, "Forward by default");
    }
    assert_eq!(host.normalized("Direction"), 0.0);
    let mut audio = vec![vec![0.0f32; 512]; 2];
    // What the host sends back, including the rounding a host might add to the editor's value.
    for (sent, expected, name) in [
        (0.5, 0.5, "Backward"),
        (1.0, 1.0, "Levitate"),
        (0.0, 0.0, "Forward"),
        (0.500_000_1, 0.5, "Backward"),
        (0.499_999_9, 0.5, "Backward"),
        (0.999_999, 1.0, "Levitate"),
    ] {
        host.set("Direction", sent);
        host.process_in(&mut audio, true, 512);
        let now = host.normalized("Direction");
        assert!((now - expected).abs() < 1e-6, "sent {sent}, the parameter reads {now}");
        assert_eq!(host.shown("Direction", now), name);
        // It stays put over the next blocks: nothing sets it back.
        host.process_in(&mut audio, true, 512);
        host.process_in(&mut audio, true, 512);
        assert!((host.normalized("Direction") - expected).abs() < 1e-6, "{name} held");
    }
}

#[test]
fn projects_load_their_direction_through_the_vst3_wrapper() {
    use nih_plug::wrapper::state::ParamValue;
    for (params, expected) in [
        (vec![("reverse", ParamValue::Bool(true))], 0.5),
        (vec![("reverse", ParamValue::Bool(false))], 0.0),
        (vec![("direction", ParamValue::String("levitate".into()))], 1.0),
        (vec![("direction", ParamValue::String("backward".into()))], 0.5),
        (vec![], 0.0),
    ] {
        let mut host = Host::new(48_000.0, 2, &project_with_params(&params));
        assert_eq!(host.normalized("Direction"), expected, "{params:?}");
        let mut audio = vec![vec![0.0f32; 512]; 2];
        host.process_in(&mut audio, true, 512);
        assert_eq!(host.normalized("Direction"), expected, "{params:?} after a block");
    }
}

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

fn tone_level(signal: &[f32], rate: f64, hz: f64) -> f32 {
    let omega = std::f32::consts::TAU * hz as f32 / rate as f32;
    let coeff = 2.0 * omega.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for sample in signal {
        let s0 = sample + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / signal.len().max(1) as f32
}

/// A loud 440 Hz partial plus a quiet 3 kHz one, on both channels.
fn partials(rate: f64, seconds: f64) -> Vec<Vec<f32>> {
    let frames = (rate * seconds) as usize;
    let side: Vec<f32> = (0..frames)
        .map(|n| {
            let t = n as f32 / rate as f32;
            (std::f32::consts::TAU * 440.0 * t).sin() * 0.5 + (std::f32::consts::TAU * 3_000.0 * t).sin() * 0.01
        })
        .collect();
    vec![side.clone(), side]
}

#[test]
fn dream_eater_off_is_untouched_and_on_strips_the_quiet_partial() {
    let rate = 48_000.0;
    let music = partials(rate, 2.0);
    let mut off_host = Host::new(rate, 2, &project_with("Water"));
    let mut off = music.clone();
    off_host.process(&mut off, false);

    let mut on_host = Host::new(rate, 2, &project_with("Water"));
    on_host.set("Dream Eater", 1.0);
    let mut on = music.clone();
    on_host.process(&mut on, false);

    assert_eq!(off[0], {
        let mut plain = music.clone();
        Host::new(rate, 2, &project_with("Water")).process(&mut plain, false);
        plain[0].clone()
    });

    // The gate delays by one 40 ms frame. Skip that and the warm-up.
    let skip = (0.25 * rate) as usize;
    let loud_off = tone_level(&off[0][skip..], rate, 440.0);
    let quiet_off = tone_level(&off[0][skip..], rate, 3_000.0);
    let loud_on = tone_level(&on[0][skip..], rate, 440.0);
    let quiet_on = tone_level(&on[0][skip..], rate, 3_000.0);
    assert!(loud_off > 0.05 && quiet_off > loud_off * 0.01, "Off keeps both partials: {loud_off} {quiet_off}");
    assert!(loud_on > 0.04, "the loud partial survives the gate: {loud_on}");
    assert!(
        quiet_on / loud_on < (quiet_off / loud_off) * 0.25,
        "the quiet partial is stripped: on {quiet_on}/{loud_on}, off {quiet_off}/{loud_off}"
    );
}

#[test]
fn turning_dream_eater_up_mid_buffer_leaves_the_start_untouched() {
    let rate = 48_000.0;
    let music = partials(rate, 2.0);
    let mut off = music.clone();
    Host::new(rate, 2, &project_with("Water")).process(&mut off, false);

    let mut host = Host::new(rate, 2, &project_with("Water"));
    let split = rate as usize;
    let mut first: Vec<Vec<f32>> = music.iter().map(|side| side[..split].to_vec()).collect();
    let mut second: Vec<Vec<f32>> = music.iter().map(|side| side[split..].to_vec()).collect();
    host.process(&mut first, true);
    host.set("Dream Eater", 1.0);
    host.process(&mut second, true);
    assert_eq!(first[0], off[0][..split], "Off leaves the output as it was");
    let difference: Vec<f32> = second[0].iter().zip(&off[0][split..]).map(|(a, b)| a - b).collect();
    let from = (0.25 * rate) as usize;
    assert!(rms(&difference[from..]) > 0.05 * rms(&off[0][split + from..]), "turning it up changes the track");
}

#[test]
fn dream_eater_adds_no_hiss_to_silence_through_the_vst3_wrapper() {
    let rate = 48_000.0;
    let frames = (rate * 2.0) as usize;
    let silence = vec![vec![0.0f32; frames]; 2];
    let mut off = silence.clone();
    let mut on = silence;
    Host::new(rate, 2, &project_with("Water")).process(&mut off, false);
    let mut host = Host::new(rate, 2, &project_with("Water"));
    host.set("Dream Eater", 1.0);
    host.process(&mut on, false);
    let peak = on.iter().zip(&off).flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (a - b).abs())).fold(0.0, f32::max);
    assert!(peak < 1.0e-5, "a silent track stays silent: {peak}");
}
