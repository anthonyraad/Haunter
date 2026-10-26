//! Haunter's audio engine. Nothing here allocates while processing.

use nih_plug::prelude::Enum;
use std::sync::Arc;

use crate::sample::PreparedSample;

/// At or below this the high-pass is bypassed.
pub const HP_OFF_HZ: f32 = 20.0;
/// At or above this the low-pass is bypassed.
pub const LP_OFF_HZ: f32 = 20_000.0;
/// The shortest region the start and end handles can make.
pub const MIN_REGION_FRAMES: usize = 32;
/// How many frames of peak analysis run per audio block.
const ANALYSIS_BUDGET: usize = 32_768;
const DECLICK_SECONDS: f64 = 0.001;

/// Dream Eater's comb delay while the sample is silent. 9 ms puts the lowest resonance near 111 Hz.
const DREAM_BASE_SECONDS: f64 = 0.009;
/// How far the sample's peak shortens the comb delay at full Dream Eater, in octaves.
const DREAM_SWEEP_OCTAVES: f64 = 3.0;
/// Comb feedback at full Dream Eater and the sample's peak. Kept below 1 so the comb always decays.
const DREAM_MAX_FEEDBACK: f64 = 0.85;
/// The feedback left between hits, as a share of the feedback at the peak.
const DREAM_RESTING_FEEDBACK: f64 = 0.35;
/// Comb level in the output at full Dream Eater. The rest is the dry track.
const DREAM_MAX_WET: f64 = 0.8;
/// Glide on the envelope before it moves the delay, so a fast attack sweeps instead of jumping.
const DREAM_GLIDE_MS: f32 = 8.0;
/// Low-pass in the comb's feedback path, so high resonances die away first.
const DREAM_DAMPING_HZ: f64 = 7_000.0;

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncRate {
    #[id = "1/1"]
    #[name = "1/1"]
    Whole,
    #[id = "1/1d"]
    #[name = "1/1 dotted"]
    WholeDotted,
    #[id = "1/1t"]
    #[name = "1/1 triplet"]
    WholeTriplet,
    #[id = "1/2"]
    #[name = "1/2"]
    Half,
    #[id = "1/2d"]
    #[name = "1/2 dotted"]
    HalfDotted,
    #[id = "1/2t"]
    #[name = "1/2 triplet"]
    HalfTriplet,
    #[id = "1/4"]
    #[name = "1/4"]
    Quarter,
    #[id = "1/4d"]
    #[name = "1/4 dotted"]
    QuarterDotted,
    #[id = "1/4t"]
    #[name = "1/4 triplet"]
    QuarterTriplet,
    #[id = "1/8"]
    #[name = "1/8"]
    Eighth,
    #[id = "1/8d"]
    #[name = "1/8 dotted"]
    EighthDotted,
    #[id = "1/8t"]
    #[name = "1/8 triplet"]
    EighthTriplet,
    #[id = "1/16"]
    #[name = "1/16"]
    Sixteenth,
    #[id = "1/16d"]
    #[name = "1/16 dotted"]
    SixteenthDotted,
    #[id = "1/16t"]
    #[name = "1/16 triplet"]
    SixteenthTriplet,
}

impl SyncRate {
    /// The grid spacing in quarter notes.
    pub fn beats(self) -> f64 {
        use SyncRate::*;
        let (straight, feel) = match self {
            Whole => (4.0, 1.0),
            WholeDotted => (4.0, 1.5),
            WholeTriplet => (4.0, 2.0 / 3.0),
            Half => (2.0, 1.0),
            HalfDotted => (2.0, 1.5),
            HalfTriplet => (2.0, 2.0 / 3.0),
            Quarter => (1.0, 1.0),
            QuarterDotted => (1.0, 1.5),
            QuarterTriplet => (1.0, 2.0 / 3.0),
            Eighth => (0.5, 1.0),
            EighthDotted => (0.5, 1.5),
            EighthTriplet => (0.5, 2.0 / 3.0),
            Sixteenth => (0.25, 1.0),
            SixteenthDotted => (0.25, 1.5),
            SixteenthTriplet => (0.25, 2.0 / 3.0),
        };
        straight * feel
    }
}

/// Fires once for every grid line, on the frame nearest to it.
#[derive(Debug, Default, Clone)]
pub struct GridClock {
    last_line: Option<i64>,
}

impl GridClock {
    pub fn reset(&mut self) {
        self.last_line = None;
    }

    /// `beat` is the song position of this frame in quarter notes. Each frame owns the half-open
    /// span `[beat - step/2, beat + step/2)`, so every grid line lands on exactly one frame even
    /// when the host's block positions jitter slightly.
    pub fn tick(&mut self, beat: f64, beats_per_frame: f64, interval: f64) -> bool {
        let half = 0.5 * beats_per_frame;
        let line = ((beat - half) / interval).ceil();
        if line * interval < beat + half {
            let line = line as i64;
            if self.last_line != Some(line) {
                self.last_line = Some(line);
                return true;
            }
        }
        false
    }
}

/// Topology-preserving-transform state variable filter, 12 dB/oct Butterworth.
#[derive(Debug, Clone, Default)]
struct Svf {
    ic1: f64,
    ic2: f64,
    a1: f64,
    a2: f64,
    a3: f64,
    k: f64,
}

impl Svf {
    fn set(&mut self, cutoff: f64, sample_rate: f64) {
        let cutoff = cutoff.clamp(1.0, sample_rate * 0.49);
        let g = (std::f64::consts::PI * cutoff / sample_rate).tan();
        self.k = std::f64::consts::SQRT_2;
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }

    /// Returns (low-pass, high-pass).
    fn process(&mut self, input: f64) -> (f64, f64) {
        let v3 = input - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        (v2, input - self.k * v1 - v2)
    }
}

/// High-pass then low-pass on the sample as it plays, before it is heard or followed.
#[derive(Debug, Clone, Default)]
pub struct SampleFilter {
    high_pass: Svf,
    low_pass: Svf,
    high_pass_on: bool,
    low_pass_on: bool,
}

impl SampleFilter {
    pub fn set(&mut self, high_pass_hz: f32, low_pass_hz: f32, sample_rate: f64) {
        let (high_pass_on, low_pass_on) = (high_pass_hz > HP_OFF_HZ, low_pass_hz < LP_OFF_HZ);
        // A bypassed filter's state is stale, so it starts from silence when switched back in.
        if high_pass_on && !self.high_pass_on {
            self.high_pass.reset();
        }
        if low_pass_on && !self.low_pass_on {
            self.low_pass.reset();
        }
        self.high_pass_on = high_pass_on;
        self.low_pass_on = low_pass_on;
        self.high_pass.set(high_pass_hz as f64, sample_rate);
        self.low_pass.set(low_pass_hz as f64, sample_rate);
    }

    pub fn reset(&mut self) {
        self.high_pass.reset();
        self.low_pass.reset();
    }

    pub fn process(&mut self, input: f64) -> f64 {
        let mut value = input;
        if self.high_pass_on {
            value = self.high_pass.process(value).1;
        }
        if self.low_pass_on {
            value = self.low_pass.process(value).0;
        }
        value
    }
}

/// Peak follower with separate attack and release time constants.
#[derive(Debug, Clone, Default)]
pub struct EnvelopeFollower {
    envelope: f64,
    attack: f64,
    release: f64,
}

fn one_pole(ms: f32, sample_rate: f64) -> f64 {
    let frames = (ms as f64 * 0.001 * sample_rate).max(1e-6);
    (-1.0 / frames).exp()
}

impl EnvelopeFollower {
    pub fn set(&mut self, attack_ms: f32, release_ms: f32, sample_rate: f64) {
        self.attack = one_pole(attack_ms, sample_rate);
        self.release = one_pole(release_ms, sample_rate);
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
    }

    pub fn process(&mut self, input: f64) -> f64 {
        let level = input.abs();
        let coefficient = if level > self.envelope { self.attack } else { self.release };
        self.envelope = level + coefficient * (self.envelope - level);
        self.envelope
    }
}

/// A feedback comb with a fractional, moving delay and damping inside the loop.
#[derive(Debug, Clone, Default)]
struct Comb {
    /// A power-of-two ring buffer of past outputs.
    buffer: Vec<f64>,
    write: usize,
    damped: f64,
}

impl Comb {
    fn allocate(&mut self, longest_delay: usize) {
        self.buffer = vec![0.0; (longest_delay + 2).next_power_of_two()];
        self.reset();
    }

    fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
        self.damped = 0.0;
    }

    /// `delay` is in frames and must be at least 1 and no longer than the allocated delay.
    /// Linear interpolation and the one-pole damping both have gains of at most 1, so the loop
    /// is stable for any `feedback` below 1 however fast the delay moves.
    fn process(&mut self, input: f64, delay: f64, feedback: f64, damping: f64) -> f64 {
        let mask = self.buffer.len() - 1;
        let whole = delay as usize;
        let fraction = delay - whole as f64;
        let newer = self.buffer[(self.write + self.buffer.len() - whole) & mask];
        let older = self.buffer[(self.write + self.buffer.len() - whole - 1) & mask];
        self.damped += damping * (newer + fraction * (older - newer) - self.damped);
        let output = input + feedback * self.damped;
        self.buffer[self.write] = output;
        self.write = (self.write + 1) & mask;
        output
    }
}

/// Dream Eater's comb settings for one frame.
#[derive(Debug, Clone, Copy)]
struct CombFrame {
    delay: f64,
    feedback: f64,
    input_scale: f64,
    wet: f64,
}

/// Everything that changes at most once per block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub rate: SyncRate,
    /// On: the region plays once from the top on every grid line. Off: it loops continuously
    /// while the transport plays, and Rate is ignored.
    pub restart: bool,
    /// Plays the region backwards, from End to Start.
    pub reverse: bool,
    /// Region start as a fraction of the sample length.
    pub start: f32,
    /// Region end as a fraction of the sample length.
    pub end: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub high_pass_hz: f32,
    pub low_pass_hz: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            rate: SyncRate::Quarter,
            restart: true,
            reverse: false,
            start: 0.0,
            end: 1.0,
            attack_ms: 1.0,
            release_ms: 80.0,
            high_pass_hz: HP_OFF_HZ,
            low_pass_hz: LP_OFF_HZ,
        }
    }
}

/// Smoothed per-frame amounts, all linear.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameGains {
    /// 0..1, how far the track ducks at the follower's peak.
    pub depth: f32,
    /// 0..1, dry/wet for the ducking.
    pub mix: f32,
    /// Sample volume times sample blend.
    pub sample_level: f32,
    /// 0..1, how hard the sample's envelope drives the comb on the track. 0 bypasses the comb.
    pub dream_eater: f32,
}

impl Default for FrameGains {
    fn default() -> Self {
        FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransportInfo {
    pub playing: bool,
    /// Song position of the block's first frame, in quarter notes.
    pub beat: Option<f64>,
    pub beats_per_frame: Option<f64>,
}

impl TransportInfo {
    pub fn playing_at(beat: f64, tempo: f64, sample_rate: f64) -> Self {
        TransportInfo { playing: true, beat: Some(beat), beats_per_frame: Some(tempo / 60.0 / sample_rate) }
    }

    pub fn stopped() -> Self {
        TransportInfo { playing: false, beat: None, beats_per_frame: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockReport {
    /// The lowest ducking gain applied to the track in this block.
    pub min_track_gain: f32,
    /// Playback position as a fraction of the sample, if a voice is sounding.
    pub playhead: Option<f32>,
    /// 0..1, how far the sample is currently sweeping the Dream Eater comb. 0 while it is off.
    pub dream_level: f32,
}

type Region = (usize, usize);

/// One pass through the region, forwards or backwards.
#[derive(Debug, Clone, Copy)]
struct Voice {
    /// Forwards, the next frame to play. Backwards, one past it, so a pass starts at `end`.
    pos: usize,
    reverse: bool,
    /// Where a fade-in started, after a trigger into the middle of the sample or a loop wrap.
    fade_in_from: Option<usize>,
}

impl Voice {
    /// Starts at the top of the region, fading in if that is mid-sample.
    fn trigger((start, end): Region, frames: usize, reverse: bool) -> Self {
        let (pos, mid_sample) = if reverse { (end, end < frames) } else { (start, start > 0) };
        Voice { pos, reverse, fade_in_from: mid_sample.then_some(pos) }
    }

    /// Starts at the top of the region again, always fading in.
    fn wrap(region: Region, frames: usize, reverse: bool) -> Self {
        let voice = Self::trigger(region, frames, reverse);
        Voice { fade_in_from: Some(voice.pos), ..voice }
    }

    /// Moves a voice that the region was dragged past to the top of the region.
    fn enter(&mut self, (start, end): Region) {
        if self.reverse && self.pos > end {
            *self = Voice { pos: end, reverse: true, fade_in_from: Some(end) };
        } else if !self.reverse && self.pos < start {
            *self = Voice { pos: start, reverse: false, fade_in_from: Some(start) };
        }
    }

    /// Frames left in the region, counting the one about to play.
    fn remaining(&self, (start, end): Region) -> usize {
        if self.reverse {
            self.pos.saturating_sub(start)
        } else {
            end.saturating_sub(self.pos)
        }
    }

    /// The sample frame to play now. Only valid while `remaining` is above zero.
    fn frame(&self) -> usize {
        if self.reverse {
            self.pos - 1
        } else {
            self.pos
        }
    }

    fn advance(&mut self) {
        if self.reverse {
            self.pos -= 1;
        } else {
            self.pos += 1;
        }
    }

    /// Short linear fades that stop clicks where playback starts or stops mid-waveform.
    fn declick_gain(&self, region: Region, fades_out: bool, declick: usize) -> f32 {
        let mut gain = 1.0;
        if let Some(from) = self.fade_in_from {
            let done = from.abs_diff(self.pos) + 1;
            if done < declick {
                gain *= done as f32 / declick as f32;
            }
        }
        let left = self.remaining(region);
        if fades_out && left < declick {
            gain *= left as f32 / declick as f32;
        }
        gain
    }
}

#[derive(Debug, Clone, Copy)]
struct Tail {
    voice: Voice,
    remaining: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct AnalysisKey {
    sample_id: u64,
    start: usize,
    end: usize,
    reverse: bool,
    fades_out: bool,
    declick: usize,
    attack_ms: f32,
    release_ms: f32,
}

#[derive(Debug, Clone)]
struct RunningAnalysis {
    key: AnalysisKey,
    voice: Voice,
    follower: EnvelopeFollower,
    peak: f64,
}

/// Finds the follower's peak over one unfiltered pass of the region, so Depth means "this much at
/// the sample's peak" and the filters can only take ducking away. Runs a few thousand frames per
/// block; settings that change mid-run start a fresh run after.
#[derive(Debug, Clone, Default)]
struct PeakAnalysis {
    done: Option<(AnalysisKey, f64)>,
    running: Option<RunningAnalysis>,
}

impl PeakAnalysis {
    fn step(&mut self, sample: &PreparedSample, wanted: AnalysisKey, sample_rate: f64, budget: usize) {
        let mut budget = budget;
        loop {
            if self.running.is_none() {
                if self.done.map(|(key, _)| key) == Some(wanted) {
                    return;
                }
                let mut follower = EnvelopeFollower::default();
                follower.set(wanted.attack_ms, wanted.release_ms, sample_rate);
                let voice = Voice::trigger((wanted.start, wanted.end), sample.frames(), wanted.reverse);
                self.running = Some(RunningAnalysis { key: wanted, voice, follower, peak: 0.0 });
            }
            let run = self.running.as_mut().unwrap();
            if run.key.sample_id != sample.id {
                self.running = None;
                continue;
            }
            let key = run.key;
            let region = (key.start, key.end);
            while budget > 0 && run.voice.remaining(region) > 0 {
                let gain = run.voice.declick_gain(region, key.fades_out, key.declick);
                let level = run.follower.process((sample.key[run.voice.frame()] * gain) as f64);
                run.peak = run.peak.max(level);
                run.voice.advance();
                budget -= 1;
            }
            if run.voice.remaining(region) > 0 {
                return;
            }
            self.done = Some((run.key, run.peak));
            self.running = None;
            if budget == 0 {
                return;
            }
        }
    }

    fn peak(&self) -> Option<f64> {
        self.done.map(|(_, peak)| peak)
    }
}

pub struct Engine {
    sample_rate: f64,
    pub settings: Settings,
    sample: Option<Arc<PreparedSample>>,
    clock: GridClock,
    voice: Option<Voice>,
    tail: Option<Tail>,
    /// Left and right.
    filters: [SampleFilter; 2],
    /// Follows the filtered sample and sets the ducking.
    follower: EnvelopeFollower,
    /// Follows the unfiltered sample, for `live_peak`.
    raw_follower: EnvelopeFollower,
    analysis: PeakAnalysis,
    /// Highest unfiltered follower level since the sample changed, used until the analysis
    /// finishes.
    live_peak: f64,
    declick: usize,
    region: Region,
    was_playing: bool,
    /// Dream Eater's combs on the track, left and right.
    combs: [Comb; 2],
    /// Whether the combs ran last frame. They start from silence each time Dream Eater comes on.
    combs_running: bool,
    /// The ducking envelope, glided, as it moves the combs.
    dream_sweep: f64,
    dream_glide: f64,
    dream_damping: f64,
    /// The comb delay while the sample is silent, in frames.
    dream_base_delay: f64,
}

impl Engine {
    pub fn new(sample_rate: f64) -> Self {
        let mut engine = Engine {
            sample_rate,
            settings: Settings::default(),
            sample: None,
            clock: GridClock::default(),
            voice: None,
            tail: None,
            filters: Default::default(),
            follower: EnvelopeFollower::default(),
            raw_follower: EnvelopeFollower::default(),
            analysis: PeakAnalysis::default(),
            live_peak: 0.0,
            declick: 1,
            region: (0, 0),
            was_playing: false,
            combs: Default::default(),
            combs_running: false,
            dream_sweep: 0.0,
            dream_glide: 0.0,
            dream_damping: 0.0,
            dream_base_delay: 1.0,
        };
        engine.prepare(sample_rate);
        engine
    }

    /// Sets the sample rate and allocates for it. Not for the audio thread.
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.declick = ((DECLICK_SECONDS * sample_rate).round() as usize).max(1);
        self.dream_base_delay = (DREAM_BASE_SECONDS * sample_rate).max(2.0);
        self.dream_glide = 1.0 - one_pole(DREAM_GLIDE_MS, sample_rate);
        self.dream_damping = 1.0 - (-2.0 * std::f64::consts::PI * DREAM_DAMPING_HZ / sample_rate).exp();
        for comb in &mut self.combs {
            comb.allocate(self.dream_base_delay.ceil() as usize);
        }
        self.reset();
    }

    pub fn reset(&mut self) {
        self.clock.reset();
        self.voice = None;
        self.tail = None;
        for filter in &mut self.filters {
            filter.reset();
        }
        self.follower.reset();
        self.raw_follower.reset();
        self.was_playing = false;
        for comb in &mut self.combs {
            comb.reset();
        }
        self.combs_running = false;
        self.dream_sweep = 0.0;
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub fn sample(&self) -> Option<&Arc<PreparedSample>> {
        self.sample.as_ref()
    }

    /// Swaps in a new sample and hands back the old one, so the caller can free it off the audio
    /// thread.
    pub fn set_sample(&mut self, sample: Option<Arc<PreparedSample>>) -> Option<Arc<PreparedSample>> {
        self.voice = None;
        self.tail = None;
        self.live_peak = 0.0;
        std::mem::replace(&mut self.sample, sample)
    }

    /// Completes the peak analysis immediately. For use outside the audio thread.
    pub fn finish_analysis(&mut self) {
        if let Some(sample) = self.sample.clone() {
            self.update_region(sample.frames());
            let key = self.analysis_key(&sample);
            self.analysis.step(&sample, key, self.sample_rate, usize::MAX);
        }
    }

    /// The region in frames, ordered and at least [`MIN_REGION_FRAMES`] long.
    pub fn region_frames(start: f32, end: f32, frames: usize) -> (usize, usize) {
        let to_frame = |fraction: f32| ((fraction.clamp(0.0, 1.0) as f64) * frames as f64).round() as usize;
        let (a, b) = (to_frame(start), to_frame(end));
        let (mut first, mut last) = (a.min(b), a.max(b));
        let shortest = MIN_REGION_FRAMES.min(frames);
        if last - first < shortest {
            last = (first + shortest).min(frames);
            first = last - shortest;
        }
        (first, last)
    }

    fn update_region(&mut self, frames: usize) {
        self.region = Self::region_frames(self.settings.start, self.settings.end, frames);
    }

    /// Whether a pass fades out at its last frame: always when looping, otherwise only when it
    /// stops mid-sample.
    fn fades_out(&self, reverse: bool, frames: usize) -> bool {
        let (start, end) = self.region;
        !self.settings.restart || if reverse { start > 0 } else { end < frames }
    }

    fn analysis_key(&self, sample: &PreparedSample) -> AnalysisKey {
        let reverse = self.settings.reverse;
        AnalysisKey {
            sample_id: sample.id,
            start: self.region.0,
            end: self.region.1,
            reverse,
            fades_out: self.fades_out(reverse, sample.frames()),
            declick: self.declick,
            attack_ms: self.settings.attack_ms,
            release_ms: self.settings.release_ms,
        }
    }

    fn normaliser(&self) -> f64 {
        match self.analysis.peak() {
            Some(peak) if peak > 1e-9 => peak,
            _ => self.live_peak,
        }
    }

    fn begin_block(&mut self) {
        let settings = self.settings;
        for filter in &mut self.filters {
            filter.set(settings.high_pass_hz, settings.low_pass_hz, self.sample_rate);
        }
        self.follower.set(settings.attack_ms, settings.release_ms, self.sample_rate);
        self.raw_follower.set(settings.attack_ms, settings.release_ms, self.sample_rate);
        if let Some(sample) = self.sample.clone() {
            self.update_region(sample.frames());
            let key = self.analysis_key(&sample);
            self.analysis.step(&sample, key, self.sample_rate, ANALYSIS_BUDGET);
        }
    }

    /// Runs Dream Eater's comb over the track, ducks `channels` in place, and mixes in the sample.
    /// `gains` is called once per frame.
    pub fn process(
        &mut self,
        channels: &mut [&mut [f32]],
        transport: &TransportInfo,
        mut gains: impl FnMut() -> FrameGains,
    ) -> BlockReport {
        self.begin_block();
        let frames = channels.first().map_or(0, |channel| channel.len());
        let channel_count = channels.len();
        let interval = self.settings.rate.beats();
        let grid = match (self.settings.restart, transport.playing, transport.beat, transport.beats_per_frame) {
            (true, true, Some(beat), Some(step)) if step > 0.0 => Some((beat, step)),
            _ => None,
        };
        if grid.is_none() {
            self.clock.reset();
        }
        // Without Restart, the loop starts from the top when the transport starts, or as soon as
        // it is needed if nothing is sounding.
        let mut start_loop = !self.settings.restart && transport.playing && (!self.was_playing || self.voice.is_none());
        self.was_playing = transport.playing;

        let mut min_track_gain = 1.0f32;
        for frame in 0..frames {
            let trigger = match grid {
                Some((beat, step)) => self.clock.tick(beat + frame as f64 * step, step, interval),
                None => std::mem::take(&mut start_loop),
            };
            let gains = gains();
            let (track_gain, add_left, add_right, envelope) = self.frame(trigger, transport.playing, &gains);
            let comb = self.dream_eater(envelope, gains.dream_eater);
            min_track_gain = min_track_gain.min(track_gain);
            for (index, channel) in channels.iter_mut().enumerate() {
                let add = match (channel_count, index) {
                    (1, _) => 0.5 * (add_left + add_right),
                    (_, 0) => add_left,
                    (_, 1) => add_right,
                    _ => 0.0,
                };
                let track = match comb {
                    Some(comb) => self.eat(index, channel[frame], comb),
                    None => channel[frame],
                };
                channel[frame] = track * track_gain + add;
            }
        }

        let playhead = match (&self.sample, &self.voice) {
            (Some(sample), Some(voice)) if sample.frames() > 0 => Some(voice.pos as f32 / sample.frames() as f32),
            _ => None,
        };
        let dream_level = if self.combs_running { self.dream_sweep.clamp(0.0, 1.0) as f32 } else { 0.0 };
        BlockReport { min_track_gain, playhead, dream_level }
    }

    /// Glides the envelope and turns it into this frame's comb settings, or `None` when Dream Eater
    /// is off and the track should pass untouched.
    fn dream_eater(&mut self, envelope: f32, amount: f32) -> Option<CombFrame> {
        self.dream_sweep += self.dream_glide * (envelope as f64 - self.dream_sweep);
        if amount.is_nan() || amount <= 0.0 {
            self.combs_running = false;
            return None;
        }
        // A comb that was bypassed holds stale audio, so it starts again from silence.
        if !self.combs_running {
            for comb in &mut self.combs {
                comb.reset();
            }
            self.combs_running = true;
        }
        // The comb's effect is roughly wet times feedback, so a square-root curve on both makes it
        // grow evenly along the knob instead of hiding in its last third.
        let drive = (amount.min(1.0) as f64).sqrt();
        let sweep = self.dream_sweep.clamp(0.0, 1.0);
        let delay = (self.dream_base_delay * (-DREAM_SWEEP_OCTAVES * drive * sweep).exp2()).max(2.0);
        let feedback =
            DREAM_MAX_FEEDBACK * drive * (DREAM_RESTING_FEEDBACK + (1.0 - DREAM_RESTING_FEEDBACK) * sweep);
        // A feedback comb lifts a tone on a resonance by 1/(1-g) and broadband sound by
        // 1/sqrt(1-g²). Scaling the input by the geometric mean of the two keeps both within about
        // 5.5 dB of the dry level at the highest feedback.
        let input_scale = ((1.0 - feedback) * (1.0 - feedback * feedback).sqrt()).sqrt();
        Some(CombFrame { delay, feedback, input_scale, wet: DREAM_MAX_WET * drive })
    }

    fn eat(&mut self, channel: usize, track: f32, frame: CombFrame) -> f32 {
        let Some(comb) = self.combs.get_mut(channel) else {
            return track;
        };
        let dry = track as f64;
        let wet = comb.process(dry * frame.input_scale, frame.delay, frame.feedback, self.dream_damping);
        if !wet.is_finite() {
            comb.reset();
            return track;
        }
        (dry + frame.wet * (wet - dry)) as f32
    }

    fn fade_gain(&self, voice: &Voice, frames: usize) -> f32 {
        voice.declick_gain(self.region, self.fades_out(voice.reverse, frames), self.declick)
    }

    /// Returns (track gain, sample left, sample right, normalised envelope) for one frame.
    fn frame(&mut self, trigger: bool, playing: bool, gains: &FrameGains) -> (f32, f32, f32, f32) {
        let Some(sample) = self.sample.clone() else {
            return (1.0, 0.0, 0.0, 0.0);
        };
        let frames = sample.frames();
        let region = self.region;
        let reverse = self.settings.reverse;

        if trigger {
            if let Some(voice) = self.voice.take() {
                self.tail = Some(Tail { voice, remaining: self.declick });
            }
            self.voice = Some(Voice::trigger(region, frames, reverse));
        }

        let (mut left, mut right, mut raw_key) = (0.0f32, 0.0f32, 0.0f32);
        if let Some(mut voice) = self.voice {
            voice.enter(region);
            if voice.remaining(region) > 0 {
                let gain = self.fade_gain(&voice, frames);
                let at = voice.frame();
                left += sample.left[at] * gain;
                right += sample.right[at] * gain;
                raw_key += sample.key[at] * gain;
                voice.advance();
            }
            self.voice = if voice.remaining(region) > 0 {
                Some(voice)
            } else if !self.settings.restart && playing {
                Some(Voice::wrap(region, frames, reverse))
            } else {
                None
            };
        }
        if let Some(mut tail) = self.tail {
            if tail.voice.remaining(region) > 0 && tail.remaining > 0 {
                let gain = self.fade_gain(&tail.voice, frames) * tail.remaining as f32 / self.declick as f32;
                let at = tail.voice.frame();
                left += sample.left[at] * gain;
                right += sample.right[at] * gain;
                raw_key += sample.key[at] * gain;
                tail.voice.advance();
                tail.remaining -= 1;
                self.tail = Some(tail);
            } else {
                self.tail = None;
            }
        }

        let left = self.filters[0].process(left as f64);
        let right = self.filters[1].process(right as f64);
        let level = self.follower.process(0.5 * (left + right));
        self.live_peak = self.live_peak.max(self.raw_follower.process(raw_key as f64));
        let peak = self.normaliser();
        let normalised = if peak > 1e-9 { (level / peak).min(1.0) as f32 } else { 0.0 };
        let ducked = 1.0 - gains.depth * normalised;
        let track_gain = 1.0 - gains.mix * (1.0 - ducked);
        (track_gain, left as f32 * gains.sample_level, right as f32 * gains.sample_level, normalised)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::EmbeddedSample;

    const RATE: f64 = 48_000.0;

    fn prepared(channels: Vec<Vec<f32>>) -> Arc<PreparedSample> {
        Arc::new(PreparedSample::new(&EmbeddedSample { name: "test".into(), sample_rate: RATE, channels }, RATE))
    }

    /// A decaying 60 Hz "kick", `ms` long.
    fn kick(ms: f64) -> Vec<f32> {
        let frames = (ms * 0.001 * RATE) as usize;
        (0..frames)
            .map(|n| {
                let t = n as f64 / RATE;
                ((2.0 * std::f64::consts::PI * 60.0 * t).sin() * (-t * 20.0).exp()) as f32
            })
            .collect()
    }

    /// A ramp sample where frame n has value n / frames, so output identifies the frame played.
    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames).map(|n| (n + 1) as f32 / frames as f32).collect()
    }

    fn process_constant(
        engine: &mut Engine,
        frames: usize,
        transport: TransportInfo,
        gains: FrameGains,
        input: f32,
    ) -> (Vec<f32>, Vec<f32>) {
        let mut left = vec![input; frames];
        let mut right = vec![input; frames];
        engine.process(&mut [&mut left[..], &mut right[..]], &transport, || gains);
        (left, right)
    }

    /// Processes a track held at 1.0, so the output is the gain applied to it.
    fn run(engine: &mut Engine, frames: usize, transport: TransportInfo, gains: FrameGains) -> (Vec<f32>, Vec<f32>) {
        process_constant(engine, frames, transport, gains, 1.0)
    }

    /// Processes a silent track, so the output is only the blended sample.
    fn play(engine: &mut Engine, frames: usize, transport: TransportInfo) -> Vec<f32> {
        process_constant(engine, frames, transport, sample_only(), 0.0).0
    }

    fn sample_only() -> FrameGains {
        FrameGains { depth: 0.0, mix: 0.0, sample_level: 1.0, dream_eater: 0.0 }
    }

    /// Frames where the grid fires, found by playing a one-frame click sample.
    fn trigger_frames(rate: SyncRate, start_beat: f64, tempo: f64, frames: usize, block: usize) -> Vec<usize> {
        let mut clock = GridClock::default();
        let step = tempo / 60.0 / RATE;
        let mut hits = Vec::new();
        let mut offset = 0;
        while offset < frames {
            let block_beat = start_beat + offset as f64 * step;
            for frame in 0..block.min(frames - offset) {
                if clock.tick(block_beat + frame as f64 * step, step, rate.beats()) {
                    hits.push(offset + frame);
                }
            }
            offset += block;
        }
        hits
    }

    #[test]
    fn rates_have_the_right_length_in_beats() {
        assert_eq!(SyncRate::Whole.beats(), 4.0);
        assert_eq!(SyncRate::Quarter.beats(), 1.0);
        assert_eq!(SyncRate::EighthDotted.beats(), 0.75);
        assert!((SyncRate::EighthTriplet.beats() - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(SyncRate::SixteenthDotted.beats(), 0.375);
        assert_eq!(SyncRate::variants().len(), 15);
    }

    #[test]
    fn grid_fires_on_every_quarter_note_at_120_bpm() {
        // 120 bpm at 48 kHz is 24000 frames per quarter note.
        let hits = trigger_frames(SyncRate::Quarter, 0.0, 120.0, 96_000, 512);
        assert_eq!(hits, vec![0, 24_000, 48_000, 72_000]);
    }

    #[test]
    fn grid_waits_for_the_next_line_when_starting_between_lines() {
        let hits = trigger_frames(SyncRate::Quarter, 0.3, 120.0, 48_000, 256);
        assert_eq!(hits, vec![16_800, 40_800]);
    }

    #[test]
    fn grid_handles_dotted_and_triplet_rates() {
        let dotted = trigger_frames(SyncRate::EighthDotted, 0.0, 120.0, 72_000, 480);
        assert_eq!(dotted, vec![0, 18_000, 36_000, 54_000]);
        let triplet = trigger_frames(SyncRate::EighthTriplet, 0.0, 120.0, 24_000, 333);
        assert_eq!(triplet, vec![0, 8_000, 16_000]);
        let whole = trigger_frames(SyncRate::Whole, 0.0, 90.0, 300_000, 1024);
        // 90 bpm: 32000 frames per beat, 128000 per bar.
        assert_eq!(whole, vec![0, 128_000, 256_000]);
    }

    #[test]
    fn grid_timing_does_not_depend_on_block_size() {
        let reference = trigger_frames(SyncRate::SixteenthTriplet, 1.234, 137.0, 100_000, 100_000);
        for block in [1, 17, 64, 511, 4096] {
            assert_eq!(trigger_frames(SyncRate::SixteenthTriplet, 1.234, 137.0, 100_000, block), reference);
        }
        // 100000 frames at 137 bpm is 4.76 beats, six lines per beat.
        assert_eq!(reference.len(), 28);
    }

    #[test]
    fn grid_retriggers_after_a_cycle_jump() {
        let mut clock = GridClock::default();
        let step = 120.0 / 60.0 / RATE;
        assert!(clock.tick(0.0, step, 1.0));
        assert!(!clock.tick(step, step, 1.0));
        assert!(clock.tick(4.0, step, 1.0));
        assert!(clock.tick(0.0, step, 1.0), "jumping back to the cycle start fires again");
    }

    #[test]
    fn engine_only_triggers_while_the_transport_plays() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![vec![0.5; 4800]])));
        let left = play(&mut engine, 4800, TransportInfo::stopped());
        assert!(left.iter().all(|&v| v == 0.0));
        let stopped_with_position = TransportInfo { playing: false, ..TransportInfo::playing_at(0.0, 120.0, RATE) };
        let left = play(&mut engine, 4800, stopped_with_position);
        assert!(left.iter().all(|&v| v == 0.0));
        let left = play(&mut engine, 4800, TransportInfo::playing_at(0.0, 120.0, RATE));
        assert_eq!(left[0], 0.5);
    }

    #[test]
    fn envelope_follower_attack_and_release_time_constants() {
        let mut follower = EnvelopeFollower::default();
        follower.set(10.0, 100.0, RATE);
        let attack_frames = (0.010 * RATE) as usize;
        let mut level = 0.0;
        for _ in 0..attack_frames {
            level = follower.process(1.0);
        }
        assert!((level - (1.0 - (-1.0f64).exp())).abs() < 1e-3, "after one attack time: {level}");
        for _ in 0..(RATE as usize) {
            follower.process(1.0);
        }
        let release_frames = (0.100 * RATE) as usize;
        for _ in 0..release_frames {
            level = follower.process(0.0);
        }
        assert!((level - (-1.0f64).exp()).abs() < 1e-3, "after one release time: {level}");
    }

    #[test]
    fn envelope_follower_rectifies_and_holds_with_slow_release() {
        let mut follower = EnvelopeFollower::default();
        follower.set(0.01, 500.0, RATE);
        let mut minimum = f64::MAX;
        for n in 0..4800 {
            let input = (2.0 * std::f64::consts::PI * 100.0 * n as f64 / RATE).sin();
            let level = follower.process(input);
            if n > 480 {
                minimum = minimum.min(level);
            }
        }
        assert!(minimum > 0.95, "a slow release rides over the zero crossings: {minimum}");
    }

    #[test]
    fn depth_sets_the_reduction_at_the_sample_peak() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(400.0)])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, dream_eater: 0.0 };
        let (left, right) = run(&mut engine, 24_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let lowest = left.iter().cloned().fold(f32::MAX, f32::min);
        assert!((lowest - 0.25).abs() < 1e-4, "75% depth leaves 25% at the peak, got {lowest}");
        assert_eq!(left, right);
        assert!(left[0] > 0.9, "the duck follows the attack, it doesn't jump");
        assert!(left[23_999] > 0.99, "the track recovers after the kick");
    }

    #[test]
    fn reverse_ducks_with_the_backwards_shape() {
        let mut engine = Engine::new(RATE);
        engine.settings.reverse = true;
        engine.set_sample(Some(prepared(vec![kick(400.0)])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, dream_eater: 0.0 };
        let (left, _) = run(&mut engine, 24_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let (deepest, lowest) =
            left.iter().cloned().enumerate().fold((0, f32::MAX), |best, (n, v)| if v < best.1 { (n, v) } else { best });
        assert!((lowest - 0.25).abs() < 1e-3, "Depth still lands at the reversed peak, got {lowest}");
        assert!(left[100] > 0.9, "the quiet tail comes first: {}", left[100]);
        assert!((18_000..19_200).contains(&deepest), "the kick's attack comes last, at frame {deepest}");
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn sine(freq: f64, frames: usize) -> Vec<f32> {
        (0..frames).map(|n| (2.0 * std::f64::consts::PI * freq * n as f64 / RATE).sin() as f32).collect()
    }

    #[test]
    fn high_and_low_pass_filter_the_blended_sample() {
        let heard = |freq: f64, high_pass_hz: f32, low_pass_hz: f32| {
            let mut engine = Engine::new(RATE);
            engine.settings.high_pass_hz = high_pass_hz;
            engine.settings.low_pass_hz = low_pass_hz;
            engine.set_sample(Some(prepared(vec![sine(freq, 12_000)])));
            rms(&play(&mut engine, 12_000, TransportInfo::playing_at(0.0, 120.0, RATE))[2_400..])
        };
        let db = |filtered: f32, open: f32| 20.0 * (filtered / open).log10();
        let low_open = heard(60.0, HP_OFF_HZ, LP_OFF_HZ);
        let high_open = heard(5_000.0, HP_OFF_HZ, LP_OFF_HZ);
        assert!((low_open - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01, "Off means unfiltered: {low_open}");
        let high_passed = db(heard(60.0, 1_000.0, LP_OFF_HZ), low_open);
        let low_passed = db(heard(5_000.0, HP_OFF_HZ, 500.0), high_open);
        assert!(high_passed < -30.0, "HP at 1 kHz removes a 60 Hz sample: {high_passed} dB");
        assert!(low_passed < -30.0, "LP at 500 Hz removes a 5 kHz sample: {low_passed} dB");
        assert!(db(heard(5_000.0, 1_000.0, LP_OFF_HZ), high_open) > -0.5, "HP passes what is above it");
    }

    #[test]
    fn high_and_low_pass_change_the_ducking() {
        let deepest = |high_pass_hz: f32, low_pass_hz: f32| {
            let mut engine = Engine::new(RATE);
            engine.settings.high_pass_hz = high_pass_hz;
            engine.settings.low_pass_hz = low_pass_hz;
            engine.set_sample(Some(prepared(vec![kick(300.0)])));
            engine.finish_analysis();
            let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0 };
            let (left, _) = run(&mut engine, 14_400, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
            left.iter().cloned().fold(f32::MAX, f32::min)
        };
        let open = deepest(HP_OFF_HZ, LP_OFF_HZ);
        let high_passed = deepest(500.0, LP_OFF_HZ);
        let low_passed = deepest(HP_OFF_HZ, 50.0);
        assert!(open < 0.01, "full depth at the unfiltered peak: {open}");
        assert!(high_passed > 0.7, "a 60 Hz kick barely ducks once high-passed at 500 Hz: {high_passed}");
        assert!(low_passed > open + 0.1 && low_passed < 0.9, "LP at 50 Hz softens the duck: {low_passed}");
    }

    #[test]
    fn switching_a_filter_back_in_starts_it_from_silence() {
        let mut filter = SampleFilter::default();
        filter.set(500.0, LP_OFF_HZ, RATE);
        for _ in 0..1_000 {
            filter.process(1.0);
        }
        filter.set(HP_OFF_HZ, LP_OFF_HZ, RATE);
        assert_eq!(filter.process(0.0), 0.0);
        filter.set(500.0, LP_OFF_HZ, RATE);
        assert_eq!(filter.process(0.0), 0.0, "no leftover state from before it was bypassed");
    }

    #[test]
    fn mix_scales_the_duck() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(400.0)])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 1.0, mix: 0.5, sample_level: 0.0, dream_eater: 0.0 };
        let (left, _) = run(&mut engine, 24_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let lowest = left.iter().cloned().fold(f32::MAX, f32::min);
        assert!((lowest - 0.5).abs() < 1e-4, "half the duck, got {lowest}");
    }

    #[test]
    fn restart_plays_the_region_once_per_grid_step() {
        let mut engine = Engine::new(RATE);
        assert!(engine.settings.restart, "Restart is on by default");
        engine.set_sample(Some(prepared(vec![ramp(1_000)])));
        let left = play(&mut engine, 25_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        for n in 0..1_000 {
            assert_eq!(left[n], (n + 1) as f32 / 1_000.0);
        }
        assert!(left[1_000..24_000].iter().all(|&v| v == 0.0), "silent after one pass");
        assert_eq!(left[24_000 + 500], 501.0 / 1_000.0, "the next quarter note starts it again");
    }

    #[test]
    fn restart_cuts_a_long_region_off_at_the_next_step() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![ramp(10_000)])));
        // 1/16 at 120 bpm is 6000 frames, shorter than the 10000-frame region.
        engine.settings.rate = SyncRate::Sixteenth;
        let left = play(&mut engine, 12_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        let declick = (DECLICK_SECONDS * RATE) as usize;
        assert_eq!(left[5_999], 6_000.0 / 10_000.0);
        // After the retrigger's crossfade, the voice is back at the start.
        assert_eq!(left[6_000 + declick + 10], (declick + 11) as f32 / 10_000.0);
    }

    #[test]
    fn without_restart_the_region_loops_and_ignores_the_grid() {
        let mut engine = Engine::new(RATE);
        engine.settings.restart = false;
        engine.settings.rate = SyncRate::Sixteenth;
        engine.set_sample(Some(prepared(vec![ramp(10_000)])));
        let left = play(&mut engine, 30_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        let declick = (DECLICK_SECONDS * RATE) as usize;
        // No restart at the 1/16 lines (every 6000 frames): the ramp just keeps going.
        for n in [5_999, 6_000, 6_000 + declick + 10, 12_000, 18_000, 24_000] {
            let expected = ((n % 10_000) + 1) as f32 / 10_000.0;
            assert_eq!(left[n], expected, "frame {n}");
        }
        assert!(left[9_999] < 1.0 && left[10_000] < 1.0 / 10_000.0, "wraps are declicked");
        assert_eq!(left[10_000 + declick + 1], (declick + 2) as f32 / 10_000.0);
        assert_eq!(left[25_000], 5_001.0 / 10_000.0, "still looping in the third pass");
    }

    #[test]
    fn without_restart_the_loop_starts_with_the_transport() {
        let mut engine = Engine::new(RATE);
        engine.settings.restart = false;
        engine.set_sample(Some(prepared(vec![ramp(1_000)])));
        assert!(play(&mut engine, 500, TransportInfo::stopped()).iter().all(|&v| v == 0.0));
        // Between grid lines, which only matter with Restart on.
        let left = play(&mut engine, 2_500, TransportInfo::playing_at(0.3, 120.0, RATE));
        assert_eq!(left[0], 1.0 / 1_000.0);
        assert_eq!(left[500], 501.0 / 1_000.0);
        assert_eq!(left[2_400], 401.0 / 1_000.0);

        // Stopping and starting again goes back to the top of the region.
        play(&mut engine, 100, TransportInfo::stopped());
        let left = play(&mut engine, 100, TransportInfo::playing_at(8.0, 120.0, RATE));
        let declick = (DECLICK_SECONDS * RATE) as usize;
        assert_eq!(left[declick + 10], (declick + 11) as f32 / 1_000.0);
    }

    #[test]
    fn without_restart_the_loop_finishes_its_pass_when_the_transport_stops() {
        let mut engine = Engine::new(RATE);
        engine.settings.restart = false;
        engine.set_sample(Some(prepared(vec![vec![0.5; 1_000]])));
        play(&mut engine, 500, TransportInfo::playing_at(0.0, 120.0, RATE));
        let left = play(&mut engine, 2_000, TransportInfo::stopped());
        assert_eq!(left[100], 0.5);
        assert!(left[500..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn reverse_plays_the_region_backwards() {
        let mut engine = Engine::new(RATE);
        engine.settings.reverse = true;
        engine.set_sample(Some(prepared(vec![ramp(1_000)])));
        let left = play(&mut engine, 2_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        // The whole sample needs no fades: it starts at the file's end and stops at its start.
        for n in 0..1_000 {
            assert_eq!(left[n], (1_000 - n) as f32 / 1_000.0, "frame {n}");
        }
        assert!(left[1_000..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn reverse_plays_a_region_from_end_to_start() {
        let mut engine = Engine::new(RATE);
        engine.settings.reverse = true;
        engine.settings.start = 0.25;
        engine.settings.end = 0.75;
        engine.set_sample(Some(prepared(vec![ramp(8_000)])));
        let left = play(&mut engine, 8_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        let declick = (DECLICK_SECONDS * RATE) as usize;
        // Frame 0 plays sample frame 5999 after a short fade-in, and the pass ends at frame 2000.
        assert!(left[0] < 6_000.0 / 8_000.0 / declick as f32 + 1e-6);
        for n in declick..(4_000 - declick) {
            assert_eq!(left[n], (6_000 - n) as f32 / 8_000.0, "frame {n}");
        }
        assert!(left[3_999] < left[4_000 - declick - 1], "fades out at the start handle");
        assert!(left[4_000..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn reverse_loops_backwards_without_restart() {
        let mut engine = Engine::new(RATE);
        engine.settings.reverse = true;
        engine.settings.restart = false;
        engine.set_sample(Some(prepared(vec![ramp(1_000)])));
        let left = play(&mut engine, 3_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        for pass in 0..3 {
            assert_eq!(left[pass * 1_000 + 300], 700.0 / 1_000.0, "pass {pass}");
        }
    }

    #[test]
    fn start_and_end_select_the_region() {
        let mut engine = Engine::new(RATE);
        engine.settings.start = 0.25;
        engine.settings.end = 0.75;
        engine.set_sample(Some(prepared(vec![ramp(8_000)])));
        let left = play(&mut engine, 8_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        let declick = (DECLICK_SECONDS * RATE) as usize;
        // Frame 0 plays sample frame 2000 (after a short fade-in), and the region is 4000 frames.
        assert!(left[0] < 2_001.0 / 8_000.0);
        for n in declick..(4_000 - declick) {
            assert_eq!(left[n], (2_000 + n + 1) as f32 / 8_000.0, "frame {n}");
        }
        assert!(left[3_999] < left[4_000 - declick - 1], "fades out at the end handle");
        assert!(left[4_000..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn swapped_or_tiny_regions_are_ordered_and_kept_playable() {
        assert_eq!(Engine::region_frames(0.75, 0.25, 8_000), (2_000, 6_000));
        assert_eq!(Engine::region_frames(0.5, 0.5, 8_000), (4_000, 4_000 + MIN_REGION_FRAMES));
        assert_eq!(Engine::region_frames(1.0, 1.0, 8_000), (8_000 - MIN_REGION_FRAMES, 8_000));
        assert_eq!(Engine::region_frames(0.0, 1.0, 10), (0, 10));
    }

    #[test]
    fn region_changes_the_ducking_shape() {
        // Two hits in one sample: a loud one, then a quiet one. Selecting only the quiet one
        // normalises it, so it ducks just as deeply.
        let mut sample = vec![0.0f32; 48_000];
        for n in 0..4_800 {
            sample[n] = (2.0 * std::f64::consts::PI * 80.0 * n as f64 / RATE).sin() as f32;
            sample[24_000 + n] = 0.2 * sample[n];
        }
        let mut engine = Engine::new(RATE);
        engine.settings.start = 0.5;
        engine.set_sample(Some(prepared(vec![sample])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0 };
        let (left, _) = run(&mut engine, 12_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let lowest = left.iter().cloned().fold(f32::MAX, f32::min);
        assert!(lowest < 1e-4, "{lowest}");
    }

    #[test]
    fn stereo_samples_blend_into_matching_channels_and_mono_tracks_get_the_sum() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![vec![0.4; 100], vec![-0.2; 100]])));
        let gains = FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.5, dream_eater: 0.0 };
        let (left, right) = run(&mut engine, 200, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        assert!((left[10] - 1.2).abs() < 1e-6 && (right[10] - 0.9).abs() < 1e-6);
        assert_eq!((left[150], right[150]), (1.0, 1.0));

        engine.reset();
        let mut mono = vec![0.0f32; 50];
        engine.process(&mut [&mut mono[..]], &TransportInfo::playing_at(0.0, 120.0, RATE), || gains);
        assert!((mono[10] - 0.05).abs() < 1e-6);
    }

    #[test]
    fn no_sample_passes_audio_through() {
        let mut engine = Engine::new(RATE);
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 1.0, dream_eater: 0.0 };
        let (left, _) = run(&mut engine, 1_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        assert!(left.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn analysis_runs_incrementally_on_the_audio_thread() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(2_000.0)])));
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0 };
        // 96000 frames of analysis at 32768 per block takes three blocks.
        for _ in 0..3 {
            run(&mut engine, 64, TransportInfo::stopped(), gains);
        }
        assert!(engine.analysis.peak().is_some());
        let peak = engine.analysis.peak().unwrap();
        engine.analysis = PeakAnalysis::default();
        engine.finish_analysis();
        assert_eq!(engine.analysis.peak(), Some(peak));
    }

    /// Uniform noise in -1..1 from a fixed seed.
    fn noise(frames: usize) -> Vec<f32> {
        let mut seed = 0x2545_f491u32;
        (0..frames)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
            })
            .collect()
    }

    /// A busy stand-in for a track: a few partials plus noise.
    fn track(frames: usize) -> Vec<f32> {
        let partials = [sine(110.0, frames), sine(233.0, frames), sine(587.0, frames)];
        let hiss = noise(frames);
        (0..frames).map(|n| 0.15 * partials.iter().map(|p| p[n]).sum::<f32>() + 0.2 * hiss[n]).collect()
    }

    /// Processes `input` on both channels and returns the left output.
    fn process_track(
        engine: &mut Engine,
        input: &[f32],
        transport: TransportInfo,
        gains: impl FnMut() -> FrameGains,
    ) -> Vec<f32> {
        let mut left = input.to_vec();
        let mut right = input.to_vec();
        engine.process(&mut [&mut left[..], &mut right[..]], &transport, gains);
        assert_eq!(left, right);
        left
    }

    fn kick_engine() -> Engine {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(300.0)])));
        engine.finish_analysis();
        engine
    }

    fn ducking(dream_eater: f32) -> FrameGains {
        FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, dream_eater }
    }

    #[test]
    fn dream_eater_off_leaves_the_track_bit_exact() {
        let input = track(48_000);
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let (gain, _) = run(&mut kick_engine(), 48_000, playing, ducking(0.0));
        let output = process_track(&mut kick_engine(), &input, playing, || ducking(0.0));
        for n in 0..input.len() {
            assert_eq!(output[n], input[n] * gain[n], "only the duck touches the track, frame {n}");
        }

        // Once it is back at Off, a comb that was running leaves no trace.
        let (mut was_on, mut never_on) = (kick_engine(), kick_engine());
        process_track(&mut was_on, &input[..24_000], playing, || ducking(1.0));
        process_track(&mut never_on, &input[..24_000], playing, || ducking(0.0));
        let next_beat = TransportInfo::playing_at(1.0, 120.0, RATE);
        assert_eq!(
            process_track(&mut was_on, &input[24_000..], next_beat, || ducking(0.0)),
            process_track(&mut never_on, &input[24_000..], next_beat, || ducking(0.0)),
        );
    }

    #[test]
    fn turning_dream_eater_up_changes_the_track_more() {
        let input = track(96_000);
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let dry = process_track(&mut kick_engine(), &input, playing, || ducking(0.0));
        let change = |amount: f32| {
            let eaten = process_track(&mut kick_engine(), &input, playing, || ducking(amount));
            let difference: Vec<f32> = eaten.iter().zip(&dry).map(|(a, b)| a - b).collect();
            rms(&difference) / rms(&dry)
        };
        let (light, half, full) = (change(0.1), change(0.5), change(1.0));
        assert!(light > 0.02, "even a little is audible: {light}");
        assert!(half > light * 1.5 && full > half * 1.2, "{light} < {half} < {full}");
    }

    #[test]
    fn the_sample_sweeps_the_comb_in_time_with_its_hits() {
        // No ducking, so the only difference between a kick and a silent sample is the sweep.
        let input = track(96_000);
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let full = || FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0 };
        let mut silent = Engine::new(RATE);
        silent.set_sample(Some(prepared(vec![vec![0.0; 14_400]])));
        silent.finish_analysis();
        let mut kicked = kick_engine();
        let still = process_track(&mut silent, &input, playing, full);
        let mut swept = Vec::new();
        let mut levels = Vec::new();
        for chunk in 0..(96_000 / 2_400) {
            let range = chunk * 2_400..(chunk + 1) * 2_400;
            let transport = TransportInfo::playing_at(range.start as f64 * 2.0 / RATE, 120.0, RATE);
            let mut left = input[range.clone()].to_vec();
            let mut right = left.clone();
            let report = kicked.process(&mut [&mut left[..], &mut right[..]], &transport, full);
            swept.extend(left);
            levels.push(report.dream_level);
        }
        let moved = |from: usize, to: usize| {
            let (mut during, mut steady) = (Vec::new(), Vec::new());
            for beat in 0..4 {
                let range = beat * 24_000 + from..beat * 24_000 + to;
                during.extend(swept[range.clone()].iter().zip(&still[range.clone()]).map(|(a, b)| a - b));
                steady.extend_from_slice(&still[range]);
            }
            rms(&during) / rms(&steady)
        };
        let on_the_hit = moved(0, 4_800);
        let between_hits = moved(21_600, 24_000);
        assert!(on_the_hit > 0.3, "the kick sweeps the comb: {on_the_hit}");
        assert!(between_hits < on_the_hit * 0.3, "and lets go after it: {between_hits} vs {on_the_hit}");
        assert!(levels[0] > 0.3 && levels[9] < 0.05, "the editor sees the sweep: {levels:?}");
    }

    #[test]
    fn dream_eater_at_full_stays_finite_bounded_and_rings_out() {
        // Worst-case material: full-scale noise, then a full-scale tone on the comb's resting
        // resonance, retriggered every 1/16 with the fastest attack.
        let mut engine = Engine::new(RATE);
        engine.settings.rate = SyncRate::Sixteenth;
        engine.settings.attack_ms = 0.1;
        engine.settings.release_ms = 5.0;
        engine.set_sample(Some(prepared(vec![kick(300.0)])));
        engine.finish_analysis();
        let full = || FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0 };
        let frames = 4 * RATE as usize;
        let mut input = noise(frames / 2);
        input.extend(sine(1.0 / DREAM_BASE_SECONDS, frames / 2));
        let mut peak = 0.0f32;
        for (block, chunk) in input.chunks(512).enumerate() {
            let transport = TransportInfo::playing_at((block * 512) as f64 * 2.0 / RATE, 120.0, RATE);
            let out = process_track(&mut engine, chunk, transport, full);
            assert!(out.iter().all(|v| v.is_finite()), "block {block}");
            peak = out.iter().fold(peak, |m, v| m.max(v.abs()));
        }
        assert!(peak < 2.0, "stays within 6 dB of a full-scale input: {peak}");

        let tail = process_track(&mut engine, &vec![0.0; RATE as usize / 2], TransportInfo::stopped(), full);
        assert!(tail[tail.len() - 2_400..].iter().all(|v| v.abs() < 1e-6), "the comb dies away");
    }
}
