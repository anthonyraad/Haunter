//! Haunter's audio engine. Nothing here allocates while processing.

use nih_plug::prelude::Enum;
use std::sync::Arc;

use crate::dream::KeyChoice;
use crate::splatter::SpectralGate;
use crate::drive::{bend_envelope, Saturator};
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

/// Which way the region plays.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// From Start to End.
    #[id = "forward"]
    #[name = "Forward"]
    Forward,
    /// From End to Start.
    #[id = "backward"]
    #[name = "Backward"]
    Backward,
    /// From Start to End, then back to Start, and so on, bouncing off each handle.
    #[id = "levitate"]
    #[name = "Levitate"]
    Levitate,
}

impl Default for Direction {
    fn default() -> Self {
        Direction::Forward
    }
}

impl Direction {
    /// Whether a pass starts at End.
    fn starts_backwards(self) -> bool {
        self == Direction::Backward
    }

    /// The one way the region plays, if it only plays one way.
    fn only_way(self) -> Option<bool> {
        match self {
            Direction::Forward => Some(false),
            Direction::Backward => Some(true),
            Direction::Levitate => None,
        }
    }

    fn bounces(self) -> bool {
        self == Direction::Levitate
    }
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
    /// The song position of the last frame seen, to notice the host jumping back.
    last_beat: Option<f64>,
}

impl GridClock {
    pub fn reset(&mut self) {
        self.last_line = None;
        self.last_beat = None;
    }

    /// `beat` is the song position of this frame in quarter notes. Each frame owns the half-open
    /// span `[beat - step/2, beat + step/2)`, so every grid line lands on exactly one frame even
    /// when the host's block positions jitter slightly.
    pub fn tick(&mut self, beat: f64, beats_per_frame: f64, interval: f64) -> bool {
        self.tick_line(beat, beats_per_frame, interval).is_some()
    }

    /// Like [`GridClock::tick`], but returns which grid line fired, counted in `interval`s from
    /// the song's start (line 0 at beat 0, negative before it), so it depends only on the song
    /// position and not on where playback started.
    pub fn tick_line(&mut self, beat: f64, beats_per_frame: f64, interval: f64) -> Option<i64> {
        // Playback only moves forward, so landing more than a quarter of a step (and at least two
        // frames) behind the last frame means the host jumped back: a cycle wrap or a locate. The
        // line it lands on fires again even if it is the one that fired last, so a cycle one step
        // long plays every pass. Smaller wobbles, such as tempo drift between blocks, change
        // nothing.
        if let Some(previous) = self.last_beat {
            if beat < previous - (0.25 * interval).max(2.0 * beats_per_frame) {
                self.last_line = None;
            }
        }
        self.last_beat = Some(beat);
        let half = 0.5 * beats_per_frame;
        let line = ((beat - half) / interval).ceil();
        if line * interval < beat + half {
            let line = line as i64;
            if self.last_line != Some(line) {
                self.last_line = Some(line);
                return Some(line);
            }
        }
        None
    }
}

/// The song position of `frame` in a block that starts at `beat`. When the host is cycling and
/// the block runs past the cycle's end, positions from the frame that owns the end onwards are
/// folded back to the cycle's start, so no step beyond the end fires and the cycle's first step
/// fires at the wrap. Hosts that end the block at the wrap never reach the fold, and without
/// cycle information the position just runs on.
pub fn grid_beat(beat: f64, frame: usize, beats_per_frame: f64, cycle: Option<(f64, f64)>) -> f64 {
    let position = beat + frame as f64 * beats_per_frame;
    let half = 0.5 * beats_per_frame;
    match cycle {
        Some((start, end)) if end - start > beats_per_frame && beat <= end - half && position > end - half => {
            start + (position - end + half).rem_euclid(end - start) - half
        }
        _ => position,
    }
}

/// Topology-preserving-transform state variable filter, 12 dB/oct Butterworth.
#[derive(Debug, Clone, Default)]
pub(crate) struct Svf {
    ic1: f64,
    ic2: f64,
    a1: f64,
    a2: f64,
    a3: f64,
    k: f64,
}

impl Svf {
    pub(crate) fn set(&mut self, cutoff: f64, sample_rate: f64) {
        let cutoff = cutoff.clamp(1.0, sample_rate * 0.49);
        let g = (std::f64::consts::PI * cutoff / sample_rate).tan();
        self.k = std::f64::consts::SQRT_2;
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    pub(crate) fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }

    /// Returns (low-pass, high-pass).
    pub(crate) fn process(&mut self, input: f64) -> (f64, f64) {
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

pub(crate) fn one_pole(ms: f32, sample_rate: f64) -> f64 {
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

/// Everything that changes at most once per block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub rate: SyncRate,
    /// On: the region plays once from the top on every grid line. Off: it loops continuously
    /// while the transport plays, and Rate is ignored.
    pub restart: bool,
    /// Forward, backward, or bouncing between the handles.
    pub direction: Direction,
    /// Region start as a fraction of the sample length.
    pub start: f32,
    /// Region end as a fraction of the sample length.
    pub end: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub high_pass_hz: f32,
    pub low_pass_hz: f32,
    /// Kept so a project saved when Dream Eater was a tuned chord still loads. The spectral
    /// gate does not read it.
    pub dream_key: KeyChoice,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            rate: SyncRate::Quarter,
            restart: true,
            direction: Direction::Forward,
            start: 0.0,
            end: 1.0,
            attack_ms: 1.0,
            release_ms: 80.0,
            high_pass_hz: HP_OFF_HZ,
            low_pass_hz: LP_OFF_HZ,
            dream_key: KeyChoice::Auto,
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
    /// 0..1, how much of the sample is blended into the output: Sample Blend.
    pub sample_level: f32,
    /// 0..1, Dream Eater: how hard the spectral gate thins the ducked track. 0 bypasses it.
    pub dream_eater: f32,
    /// 0..1, Drive: saturates the blended sample and thickens the dips.
    /// 0 leaves everything as it was, bit for bit.
    pub drive: f32,
}

impl Default for FrameGains {
    fn default() -> Self {
        FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransportInfo {
    pub playing: bool,
    /// Song position of the block's first frame, in quarter notes.
    pub beat: Option<f64>,
    pub beats_per_frame: Option<f64>,
    /// The host's cycle (loop) in quarter notes, start included and end not, while it is on and
    /// the host says so.
    pub cycle: Option<(f64, f64)>,
}

impl TransportInfo {
    pub fn playing_at(beat: f64, tempo: f64, sample_rate: f64) -> Self {
        TransportInfo { playing: true, beat: Some(beat), beats_per_frame: Some(tempo / 60.0 / sample_rate), cycle: None }
    }

    pub fn stopped() -> Self {
        TransportInfo { playing: false, beat: None, beats_per_frame: None, cycle: None }
    }

    /// The same transport with the host cycling from `start` to `end`.
    pub fn cycling(self, start: f64, end: f64) -> Self {
        TransportInfo { cycle: Some((start, end)), ..self }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockReport {
    /// The lowest ducking gain applied to the track in this block.
    pub min_track_gain: f32,
    /// Playback position as a fraction of the sample, if a voice is sounding.
    pub playhead: Option<f32>,
    /// 0..1, how far Dream Eater's spectral gate has opened. 0 while it is off.
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

    /// Turns round where it is, so the next frame is the neighbour of the last one played, on
    /// the side it came from.
    fn turn(&mut self) {
        self.reverse = !self.reverse;
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
    direction: Direction,
    fades_out: bool,
    /// Levitate with Restart on: one pass forward, then a separate pass back from End, which
    /// fades out like this.
    alternates: Option<bool>,
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
    /// Levitate has turned at End and is on its way back.
    turned: bool,
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
                let voice = Voice::trigger((wanted.start, wanted.end), sample.frames(), wanted.direction.starts_backwards());
                self.running = Some(RunningAnalysis { key: wanted, voice, follower, peak: 0.0, turned: false });
            }
            let run = self.running.as_mut().unwrap();
            if run.key.sample_id != sample.id {
                self.running = None;
                continue;
            }
            let key = run.key;
            let region = (key.start, key.end);
            while budget > 0 && run.voice.remaining(region) > 0 {
                let fades_out = match (key.alternates, run.turned) {
                    (Some(back_fades_out), true) => back_fades_out,
                    _ => key.fades_out,
                };
                let gain = run.voice.declick_gain(region, fades_out, key.declick);
                let level = run.follower.process((sample.key[run.voice.frame()] * gain) as f64);
                run.peak = run.peak.max(level);
                run.voice.advance();
                budget -= 1;
                // Levitate's peak is over the way there and back.
                if key.direction.bounces() && !run.turned && run.voice.remaining(region) == 0 {
                    if key.alternates.is_some() {
                        // With Restart on, the way back is its own pass, from End.
                        run.voice = Voice::trigger(region, sample.frames(), true);
                    } else {
                        run.voice.turn();
                        run.voice.fade_in_from = None;
                    }
                    run.turned = true;
                }
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
    /// Dream Eater: Hyperchroma's Splatter gate, on the incoming track.
    gate: SpectralGate,
    /// Scratch for one block: the incoming track, the blended sample, and the knob.
    ducked_l: Vec<f32>,
    ducked_r: Vec<f32>,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    wet_l: Vec<f32>,
    wet_r: Vec<f32>,
    amounts: Vec<f32>,
    track_gains: Vec<f32>,
    /// 0 where this sample is still in live time, 1 where it has the gate's frame of delay.
    align: Vec<f32>,
    silence: Vec<f32>,
    /// Duck gain and blended sample, delayed by one spectral frame so they stay with the gated track.
    gain_delay: Vec<f32>,
    add_delay_l: Vec<f32>,
    add_delay_r: Vec<f32>,
    delay_pos: usize,
    /// The gate is in the output path, so this block is delayed by one STFT frame.
    splatter_on: bool,
    /// Samples left in the crossfade that opens the gate onto a primed delay line.
    fade_in: usize,
    /// Samples left in the crossfade back to the undelayed track once the gate has closed.
    fade_out: usize,
    /// Drive on the blended sample.
    saturator: Saturator,
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
            gate: SpectralGate::new(sample_rate as f32),
            ducked_l: Vec::new(),
            ducked_r: Vec::new(),
            dry_l: Vec::new(),
            dry_r: Vec::new(),
            wet_l: Vec::new(),
            wet_r: Vec::new(),
            amounts: Vec::new(),
            track_gains: Vec::new(),
            align: Vec::new(),
            silence: Vec::new(),
            gain_delay: Vec::new(),
            add_delay_l: Vec::new(),
            add_delay_r: Vec::new(),
            delay_pos: 0,
            splatter_on: false,
            fade_in: 0,
            fade_out: 0,
            saturator: Saturator::new(sample_rate),
        };
        engine.prepare(sample_rate);
        engine
    }

    /// Sets the sample rate and allocates for it. Not for the audio thread.
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.declick = ((DECLICK_SECONDS * sample_rate).round() as usize).max(1);
        self.gate.set_sample_rate(sample_rate as f32);
        self.saturator.prepare(sample_rate);
        self.reset();
    }

    /// The duck and the blended sample wait one spectral frame, matching the gate's delay.
    fn resize_duck_delay(&mut self) {
        let n = self.gate.latency_samples() as usize;
        if self.gain_delay.len() == n {
            self.gain_delay.fill(1.0);
            self.add_delay_l.fill(0.0);
            self.add_delay_r.fill(0.0);
        } else {
            self.gain_delay = vec![1.0; n];
            self.add_delay_l = vec![0.0; n];
            self.add_delay_r = vec![0.0; n];
        }
        self.delay_pos = 0;
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
        self.gate.reset();
        self.splatter_on = false;
        self.fade_in = 0;
        self.fade_out = 0;
        self.resize_duck_delay();
        self.saturator.reset();
    }

    /// How many samples the output lags while Dream Eater is open. Off is zero.
    pub fn latency_samples(&self) -> u32 {
        if self.splatter_on { self.gate.latency_samples() } else { 0 }
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

    /// Whether Levitate turns round at the handles: with Restart off, while the transport plays.
    /// With Restart on it alternates instead, one pass per Rate step (see [`Engine::plays_backwards`]).
    fn bounces(&self, playing: bool) -> bool {
        self.settings.direction.bounces() && playing && !self.settings.restart
    }

    /// Whether a pass triggered on grid line `line` plays backwards. With Restart on, Levitate
    /// alternates by the line's parity, counted from the song's start: even lines (the song's
    /// first step is line 0) play forward from Start, odd ones backward from End. So the same
    /// step always goes the same way, across loops and wherever playback starts.
    fn plays_backwards(&self, line: Option<i64>) -> bool {
        match (self.settings.direction, line) {
            (Direction::Levitate, Some(line)) if self.settings.restart => line.rem_euclid(2) == 1,
            (direction, _) => direction.starts_backwards(),
        }
    }

    /// Whether a pass fades out at its last frame: always when looping, otherwise only when it
    /// stops mid-sample. Levitate without Restart turns round at the handles instead while the
    /// transport plays, which needs no fade: the frames either side of a turn are neighbours in
    /// the sample.
    fn fades_out(&self, reverse: bool, frames: usize, playing: bool) -> bool {
        if self.bounces(playing) {
            return false;
        }
        let (start, end) = self.region;
        !self.settings.restart || if reverse { start > 0 } else { end < frames }
    }

    fn analysis_key(&self, sample: &PreparedSample) -> AnalysisKey {
        let direction = self.settings.direction;
        AnalysisKey {
            sample_id: sample.id,
            start: self.region.0,
            end: self.region.1,
            direction,
            fades_out: self.fades_out(direction.starts_backwards(), sample.frames(), true),
            alternates: (direction.bounces() && self.settings.restart).then(|| self.fades_out(true, sample.frames(), true)),
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

    /// Runs Dream Eater's spectral gate on the incoming track, then ducks that with the sample
    /// envelope and mixes the sample in. `gains` is called once per frame.
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

        self.fit_scratch(frames);
        let mut min_track_gain = 1.0f32;
        for frame in 0..frames {
            // The grid line that fired, if one did. Levitate with Restart on goes by its parity.
            let (trigger, line) = match grid {
                Some((beat, step)) => match self.clock.tick_line(grid_beat(beat, frame, step, transport.cycle), step, interval) {
                    Some(line) => (true, Some(line)),
                    None => (false, None),
                },
                None => (std::mem::take(&mut start_loop), None),
            };
            let gains = gains();
            let (track_gain, add_left, add_right) = self.frame(trigger, line, transport.playing, &gains);
            min_track_gain = min_track_gain.min(track_gain);
            let left = channels.first().map_or(0.0, |channel| channel[frame]);
            let right = channels.get(1).map_or(left, |channel| channel[frame]);
            self.track_gains[frame] = track_gain;
            self.amounts[frame] = gains.dream_eater;
            // The gate hears the incoming track at full level. The duck is a gain afterwards.
            if channel_count <= 1 {
                let add = 0.5 * (add_left + add_right);
                self.ducked_l[frame] = left;
                self.ducked_r[frame] = left;
                self.dry_l[frame] = add;
                self.dry_r[frame] = add;
            } else {
                self.ducked_l[frame] = left;
                self.ducked_r[frame] = right;
                self.dry_l[frame] = add_left;
                self.dry_r[frame] = add_right;
            }
        }
        let dream_level = self.splatter(frames);

        for (index, channel) in channels.iter_mut().enumerate() {
            for frame in 0..frames {
                channel[frame] = match index {
                    0 => self.wet_l[frame],
                    1 => self.wet_r[frame],
                    _ => channel[frame] * self.track_gains[frame],
                };
            }
        }

        let playhead = match (&self.sample, &self.voice) {
            (Some(sample), Some(voice)) if sample.frames() > 0 => Some(voice.pos as f32 / sample.frames() as f32),
            _ => None,
        };
        BlockReport { min_track_gain, playhead, dream_level }
    }

    fn fit_scratch(&mut self, frames: usize) {
        if self.wet_l.len() >= frames {
            return;
        }
        let n = frames.max(self.wet_l.len().saturating_mul(2)).max(1);
        self.ducked_l.resize(n, 0.0);
        self.ducked_r.resize(n, 0.0);
        self.dry_l.resize(n, 0.0);
        self.dry_r.resize(n, 0.0);
        self.wet_l.resize(n, 0.0);
        self.wet_r.resize(n, 0.0);
        self.amounts.resize(n, 0.0);
        self.track_gains.resize(n, 1.0);
        self.align.resize(n, 0.0);
        self.silence.resize(n, 0.0);
    }

    /// Gates the incoming track. The sample envelope then ducks that result, and the blended
    /// sample is added on top. Both the duck and the sample take the gate's delay while it is
    /// open, so the dips stay as sharp as they are with Dream Eater off. Off is a bit-exact
    /// copy of incoming × duck + sample, with no delay.
    fn splatter(&mut self, frames: usize) -> f32 {
        if frames == 0 {
            return 0.0;
        }
        let requested = self.amounts[..frames].iter().any(|amount| *amount > 1.0e-5);
        let incoming = |engine: &Engine, frame: usize, right: bool| {
            if right { engine.ducked_r[frame] } else { engine.ducked_l[frame] }
        };
        if !self.splatter_on && !requested {
            for frame in 0..frames {
                self.wet_l[frame] = incoming(self, frame, false);
                self.wet_r[frame] = incoming(self, frame, true);
                self.align[frame] = 0.0;
            }
            self.gate.feed(
                &self.ducked_l[..frames],
                &self.ducked_r[..frames],
                &self.silence[..frames],
                &self.silence[..frames],
            );
            self.apply_duck(frames);
            return 0.0;
        }
        if !self.splatter_on {
            self.splatter_on = true;
            if self.gate.primed() {
                self.fade_in = self.gate.hop_samples().max(1);
            }
        }
        self.wet_l[..frames].copy_from_slice(&self.ducked_l[..frames]);
        self.wet_r[..frames].copy_from_slice(&self.ducked_r[..frames]);
        self.align[..frames].fill(1.0);
        // First sample of the stretch that ends in bypass. Fading from there leaves the
        // samples the gate was still thinning alone.
        let closed_at = self.gate.process_varying(
            &mut self.wet_l[..frames],
            &mut self.wet_r[..frames],
            &self.silence[..frames],
            &self.silence[..frames],
            &self.amounts[..frames],
        );

        let hop = self.gate.hop_samples().max(1) as f32;
        for frame in 0..frames {
            if self.fade_in == 0 {
                break;
            }
            let x = 1.0 - (self.fade_in - 1) as f32 / hop;
            let (left, right) = (incoming(self, frame, false), incoming(self, frame, true));
            self.wet_l[frame] = left * (1.0 - x) + self.wet_l[frame] * x;
            self.wet_r[frame] = right * (1.0 - x) + self.wet_r[frame] * x;
            self.align[frame] = x;
            self.fade_in -= 1;
        }
        if requested {
            self.fade_out = 0;
        } else if let Some(at) = closed_at.filter(|_| self.fade_in == 0) {
            if self.fade_out == 0 {
                self.fade_out = self.gate.hop_samples().max(1);
            }
            for frame in at..frames {
                if self.fade_out == 0 {
                    break;
                }
                let x = self.fade_out as f32 / hop;
                let (left, right) = (incoming(self, frame, false), incoming(self, frame, true));
                self.wet_l[frame] = left * (1.0 - x) + self.wet_l[frame] * x;
                self.wet_r[frame] = right * (1.0 - x) + self.wet_r[frame] * x;
                self.align[frame] = x;
                self.fade_out -= 1;
                if self.fade_out == 0 {
                    self.splatter_on = false;
                    for rest in frame + 1..frames {
                        self.wet_l[rest] = incoming(self, rest, false);
                        self.wet_r[rest] = incoming(self, rest, true);
                        self.align[rest] = 0.0;
                    }
                    break;
                }
            }
        }
        self.apply_duck(frames);
        if self.splatter_on { self.gate.amount() } else { 0.0 }
    }

    /// Ducks the gated track and adds the blended sample. `align` is 1 where the track has the
    /// gate's delay, so the duck and the sample take that same delay and the dips stay put.
    fn apply_duck(&mut self, frames: usize) {
        for frame in 0..frames {
            let (delayed_gain, delayed_l, delayed_r) = self.shift_duck(self.track_gains[frame], self.dry_l[frame], self.dry_r[frame]);
            let x = self.align[frame];
            let gain = self.track_gains[frame] * (1.0 - x) + delayed_gain * x;
            let add_l = self.dry_l[frame] * (1.0 - x) + delayed_l * x;
            let add_r = self.dry_r[frame] * (1.0 - x) + delayed_r * x;
            self.wet_l[frame] = self.wet_l[frame] * gain + add_l;
            self.wet_r[frame] = self.wet_r[frame] * gain + add_r;
        }
    }

    /// Reads the duck and sample from one spectral frame ago and stores the current ones.
    fn shift_duck(&mut self, gain: f32, left: f32, right: f32) -> (f32, f32, f32) {
        let n = self.gain_delay.len();
        if n == 0 {
            return (gain, left, right);
        }
        let index = self.delay_pos;
        let earlier = (self.gain_delay[index], self.add_delay_l[index], self.add_delay_r[index]);
        self.gain_delay[index] = gain;
        self.add_delay_l[index] = left;
        self.add_delay_r[index] = right;
        self.delay_pos += 1;
        if self.delay_pos == n {
            self.delay_pos = 0;
        }
        earlier
    }

    fn fade_gain(&self, voice: &Voice, frames: usize, playing: bool) -> f32 {
        voice.declick_gain(self.region, self.fades_out(voice.reverse, frames, playing), self.declick)
    }

    /// Returns (track gain, sample left, sample right) for one frame.
    fn frame(&mut self, trigger: bool, line: Option<i64>, playing: bool, gains: &FrameGains) -> (f32, f32, f32) {
        let Some(sample) = self.sample.clone() else {
            if self.saturator.engaged() {
                self.saturator.reset();
            }
            return (1.0, 0.0, 0.0);
        };
        let frames = sample.frames();
        let region = self.region;
        let direction = self.settings.direction;
        let reverse = self.plays_backwards(line);

        if trigger {
            if let Some(voice) = self.voice.take() {
                self.tail = Some(Tail { voice, remaining: self.declick });
            }
            self.voice = Some(Voice::trigger(region, frames, reverse));
        }

        let (mut left, mut right, mut raw_key) = (0.0f32, 0.0f32, 0.0f32);
        if let Some(mut voice) = self.voice {
            voice.enter(region);
            // A new direction takes over at once from where the voice is, rather than at the next
            // pass. The old way fades out as a tail while the new way fades in from the same spot,
            // their gains adding up to one throughout.
            if direction.only_way().is_some_and(|backwards| backwards != voice.reverse) {
                self.tail = Some(Tail { voice, remaining: self.declick.saturating_sub(1) });
                voice.turn();
                voice.fade_in_from = Some(voice.pos);
            }
            if voice.remaining(region) > 0 {
                let gain = self.fade_gain(&voice, frames, playing);
                let at = voice.frame();
                left += sample.left[at] * gain;
                right += sample.right[at] * gain;
                raw_key += sample.key[at] * gain;
                voice.advance();
            }
            if voice.remaining(region) == 0 && self.bounces(playing) {
                voice.turn();
                // No fade-in on the way back past where it started.
                voice.fade_in_from = None;
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
                let gain = self.fade_gain(&tail.voice, frames, playing) * tail.remaining as f32 / self.declick as f32;
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
        // Drive bends the envelope as it bends the sample: the dips fill out and hold longer, but
        // reach no deeper. Dream Eater hears the track before this gain, then the bent duck is applied.
        let normalised = bend_envelope(normalised, gains.drive);
        let ducked = 1.0 - gains.depth * normalised;
        let track_gain = 1.0 - gains.mix * (1.0 - ducked);
        // The blended sample is saturated after HP and LP, and only while it is blended in. The
        // follower above hears it clean, so Drive moves the dips only through the bend.
        let drive = if gains.sample_level > 0.0 { gains.drive as f64 } else { 0.0 };
        let [left, right] = self.saturator.process([left, right], drive);
        (track_gain, left as f32 * gains.sample_level, right as f32 * gains.sample_level)
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
        FrameGains { depth: 0.0, mix: 0.0, sample_level: 1.0, dream_eater: 0.0, drive: 0.0 }
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
    /// Blocks a host playing at 120 bpm from `start` reports, as (first frame, length, beat),
    /// wrapping at the end of `cycle` like Cubase. With `split` every block ends at the wrap;
    /// otherwise blocks run across it and the next one starts inside the cycle.
    fn host_blocks(start: f64, cycle: Option<(f64, f64)>, frames: usize, block: usize, split: bool) -> Vec<(usize, usize, f64)> {
        let step = 120.0 / 60.0 / RATE;
        let (mut blocks, mut offset, mut beat) = (Vec::new(), 0, start);
        while offset < frames {
            let mut length = block.min(frames - offset);
            if let (true, Some((_, end))) = (split, cycle) {
                let left = ((end - beat) / step).round();
                if left >= 1.0 {
                    length = length.min(left as usize);
                }
            }
            blocks.push((offset, length, beat));
            let inside = cycle.map_or(false, |(_, end)| beat < end);
            beat += length as f64 * step;
            if let Some((cycle_start, end)) = cycle {
                if inside && beat >= end - 1e-9 {
                    beat = cycle_start + (beat - end).max(0.0);
                }
            }
            offset += length;
        }
        blocks
    }

    /// The grid lines that fire, as (frame, line), when `rate` plays through `blocks` at 120 bpm
    /// with `cycle` passed on to the grid.
    fn grid_lines(rate: SyncRate, blocks: &[(usize, usize, f64)], cycle: Option<(f64, f64)>) -> Vec<(usize, i64)> {
        let step = 120.0 / 60.0 / RATE;
        let mut clock = GridClock::default();
        let mut lines = Vec::new();
        for &(offset, length, beat) in blocks {
            for frame in 0..length {
                if let Some(line) = clock.tick_line(grid_beat(beat, frame, step, cycle), step, rate.beats()) {
                    lines.push((offset + frame, line));
                }
            }
        }
        lines
    }

    #[test]
    fn a_cycle_one_step_long_fires_every_pass() {
        // Bar 2 cycled at 1/1: one step, line 1, every 96,000 frames at 120 bpm.
        let cycle = Some((4.0, 8.0));
        let passes: Vec<(usize, i64)> = (0..5).map(|pass| (pass * 96_000, 1)).collect();
        for block in [512, 700, 4096] {
            let split = host_blocks(4.0, cycle, 5 * 96_000, block, true);
            assert_eq!(grid_lines(SyncRate::Whole, &split, cycle), passes, "blocks of {block} split at the wrap");
            assert_eq!(grid_lines(SyncRate::Whole, &split, None), passes, "blocks of {block} split, no cycle info");
            let across = host_blocks(4.0, cycle, 5 * 96_000, block, false);
            assert_eq!(grid_lines(SyncRate::Whole, &across, cycle), passes, "blocks of {block} across the wrap");
        }
    }

    #[test]
    fn a_cycle_fires_its_steps_in_order_and_none_past_its_end() {
        // Bars 2 to 4 cycled at 1/1: lines 1, 2, 3, then 1 again, never 4.
        let cycle = Some((4.0, 16.0));
        let expected: Vec<(usize, i64)> = (0..9).map(|n| (n * 96_000, 1 + (n % 3) as i64)).collect();
        for block in [512, 700, 4096] {
            for split in [true, false] {
                let blocks = host_blocks(4.0, cycle, 9 * 96_000, block, split);
                assert_eq!(grid_lines(SyncRate::Whole, &blocks, cycle), expected, "blocks of {block}, split {split}");
            }
            let split = host_blocks(4.0, cycle, 9 * 96_000, block, true);
            assert_eq!(grid_lines(SyncRate::Whole, &split, None), expected, "blocks of {block} split, no cycle info");
        }
        // Dotted quarters over bars 2 and 3: lines 3 to 7 each pass. Line 8 sits exactly on the
        // cycle's end and must not fire there.
        let cycle = Some((4.0, 12.0));
        let expected: Vec<(usize, i64)> =
            (0..3).flat_map(|pass| (0..5).map(move |k| (pass * 192_000 + 12_000 + k * 36_000, 3 + k as i64))).collect();
        for block in [512, 700, 4096] {
            for split in [true, false] {
                let blocks = host_blocks(4.0, cycle, 3 * 192_000, block, split);
                assert_eq!(grid_lines(SyncRate::QuarterDotted, &blocks, cycle), expected, "dotted, blocks of {block}, split {split}");
            }
        }
    }

    #[test]
    fn continuous_play_fires_every_line_once_through_tempo_changes() {
        // The tempo and block size change every block, and with a lag the host reports each
        // block a little behind where the last one ran to, as when the tempo drifts. A cycle
        // that playback is past changes nothing.
        for rate in [SyncRate::Quarter, SyncRate::SixteenthTriplet, SyncRate::EighthDotted, SyncRate::Whole] {
            for cycle in [None, Some((16.0, 32.0))] {
                for lag in [0.0, 1.5] {
                    let mut clock = GridClock::default();
                    let mut beat = 100.0;
                    let mut lines = Vec::new();
                    for index in 0..3_000 {
                        let tempo = 120.0 + 50.0 * (index as f64 * 0.05).sin();
                        let step = tempo / 60.0 / RATE;
                        let block = 64 + (index * 37) % 900;
                        for frame in 0..block {
                            if let Some(line) = clock.tick_line(grid_beat(beat, frame, step, cycle), step, rate.beats()) {
                                lines.push(line);
                            }
                        }
                        beat += (block as f64 - lag) * step;
                    }
                    let first = (100.0 / rate.beats()).ceil() as i64;
                    assert_eq!(lines[0], first, "{rate:?} {cycle:?} lag {lag}");
                    // About 64 beats in all.
                    assert!(lines.windows(2).all(|pair| pair[1] == pair[0] + 1), "{rate:?} {cycle:?} lag {lag}: a line doubled or went missing");
                }
            }
        }
    }

    fn play_grid(clock: &mut GridClock, from: f64, frames: usize) -> Vec<i64> {
        let step = 120.0 / 60.0 / RATE;
        (0..frames).filter_map(|frame| clock.tick_line(from + frame as f64 * step, step, 1.0)).collect()
    }

    #[test]
    fn locating_fires_the_line_it_lands_on() {
        let mut clock = GridClock::default();
        assert_eq!(play_grid(&mut clock, 0.0, 84_000), vec![0, 1, 2, 3], "beats 0 to 3.5");
        assert_eq!(play_grid(&mut clock, 3.0, 12_000), vec![3], "back onto the line that just fired: it fires again");
        assert_eq!(play_grid(&mut clock, 3.25, 24_000), vec![4], "back to just past a line: nothing until the next");
        assert_eq!(play_grid(&mut clock, 1.0, 1_000), vec![1], "back over several lines");
        assert_eq!(play_grid(&mut clock, 7.0, 1_000), vec![7], "forward");
        assert_eq!(play_grid(&mut clock, 7.0 + 1_000.0 * 120.0 / 60.0 / RATE, 1_000), Vec::<i64>::new(), "and on from there");
    }

    #[test]
    fn restart_plays_every_pass_of_a_cycle_one_step_long() {
        // Bar 2 cycled at 1/1, blocks running across the wrap: every pass plays the region.
        let mut engine = Engine::new(RATE);
        engine.settings.rate = SyncRate::Whole;
        engine.set_sample(Some(prepared(vec![ramp(1_000)])));
        let mut left = Vec::new();
        for (_, length, beat) in host_blocks(4.0, Some((4.0, 8.0)), 3 * 96_000, 700, false) {
            left.extend(play(&mut engine, length, TransportInfo::playing_at(beat, 120.0, RATE).cycling(4.0, 8.0)));
        }
        for pass in 0..3 {
            assert_eq!(left[pass * 96_000 + 500], 501.0 / 1_000.0, "pass {pass}");
        }
    }

    #[test]
    fn levitate_in_a_cycle_goes_by_each_steps_parity_with_no_step_past_the_end() {
        // Bars 2 to 4 cycled at 1/1 with blocks across the wrap: lines 1, 2, 3, 1, 2, 3 play
        // backward, forward, backward, backward, forward, backward. A stray line 4 at the wrap
        // would play forward there.
        let mut engine = levitate_with_restart(0.25);
        engine.settings.rate = SyncRate::Whole;
        let mut left = Vec::new();
        for (_, length, beat) in host_blocks(4.0, Some((4.0, 16.0)), 6 * 96_000, 700, false) {
            left.extend(play(&mut engine, length, TransportInfo::playing_at(beat, 120.0, RATE).cycling(4.0, 16.0)));
        }
        for step in 0..6 {
            let at = step * 96_000;
            let backwards = left[at + 3_000] > left[at + 6_000];
            assert_eq!(backwards, step % 3 != 1, "step {step}");
        }
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
        let gains = FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 };
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
        engine.settings.direction = Direction::Backward;
        engine.set_sample(Some(prepared(vec![kick(400.0)])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 };
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
            let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 };
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
        let gains = FrameGains { depth: 1.0, mix: 0.5, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 };
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
        engine.settings.direction = Direction::Backward;
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
        engine.settings.direction = Direction::Backward;
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

    /// Plays `frames` in one block and returns the left channel and where the playhead ended.
    fn play_block(engine: &mut Engine, frames: usize) -> (Vec<f32>, f32) {
        let mut left = vec![0.0; frames];
        let mut right = vec![0.0; frames];
        let report = engine.process(&mut [&mut left[..], &mut right[..]], &TransportInfo::playing_at(0.0, 120.0, RATE), sample_only);
        (left, report.playhead.expect("a voice is playing"))
    }

    /// The biggest step from one output frame to the next.
    fn biggest_step(output: &[f32]) -> f32 {
        output.windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0, f32::max)
    }

    fn loud_sine(frames: usize, hz: f64) -> Vec<f32> {
        (0..frames).map(|n| (2.0 * std::f64::consts::PI * hz * n as f64 / RATE).sin() as f32 * 0.8).collect()
    }

    #[test]
    fn choosing_another_direction_turns_the_playhead_round_within_the_next_block() {
        let mut engine = Engine::new(RATE);
        engine.settings.restart = false;
        engine.set_sample(Some(prepared(vec![ramp(48_000)])));
        let (forward, before) = play_block(&mut engine, 4_800);
        assert!((before - 0.1).abs() < 1e-4, "{before}");
        assert!(forward[4_799] > forward[4_000]);

        engine.settings.direction = Direction::Backward;
        let (backward, after) = play_block(&mut engine, 512);
        assert!(after < before - 0.01, "the playhead went back from {before} to {after}");
        assert!(backward[100] > backward[400], "the sample plays backwards from where it was");
        assert!((backward[100] - 4_700.0 / 48_000.0).abs() < 1e-4, "{}", backward[100]);
        let joined: Vec<f32> = forward[4_700..].iter().chain(&backward).copied().collect();
        assert!(biggest_step(&joined) < 1e-4, "no jump at the turn: {}", biggest_step(&joined));

        engine.settings.direction = Direction::Forward;
        let (forward_again, last) = play_block(&mut engine, 512);
        assert!(last > after + 0.005, "and forward again: {after} to {last}");
        assert!(forward_again[400] > forward_again[100]);
    }

    #[test]
    fn turning_round_does_not_click() {
        // A loud 3 kHz tone, whose own steps are big, turned round at a few awkward moments.
        let tone = loud_sine(48_000, 3_000.0);
        let natural = biggest_step(&tone);
        let mut engine = Engine::new(RATE);
        engine.settings.restart = false;
        engine.set_sample(Some(prepared(vec![tone])));
        let mut output = Vec::new();
        for (block, direction) in [(1_001, Direction::Backward), (333, Direction::Forward), (517, Direction::Backward), (2_000, Direction::Forward)] {
            output.extend(play_block(&mut engine, block).0);
            engine.settings.direction = direction;
        }
        output.extend(play_block(&mut engine, 500).0);
        let step = biggest_step(&output[50..]);
        // A click would be a jump of up to twice the tone's height; the 1 ms crossfade adds a little
        // to the tone's own steps at most.
        assert!(step <= natural * 1.15, "{step} against the tone's own {natural}");
    }

    #[test]
    fn levitate_bounces_between_the_handles() {
        let mut engine = Engine::new(RATE);
        engine.settings.direction = Direction::Levitate;
        engine.settings.restart = false;
        engine.set_sample(Some(prepared(vec![ramp(1_000)])));
        let mut left = Vec::new();
        let mut heads = Vec::new();
        for _ in 0..16 {
            let (block, head) = play_block(&mut engine, 250);
            left.extend(block);
            heads.push(head);
        }
        // Up from S to E, back down to S, and up again, without fading at the turns.
        assert_eq!(left[500], 501.0 / 1_000.0);
        assert_eq!(left[999], 1.0);
        assert_eq!(left[1_000], 1.0, "the turn at E repeats the last frame");
        assert_eq!(left[1_500], 500.0 / 1_000.0);
        assert_eq!(left[1_999], 1.0 / 1_000.0);
        assert_eq!(left[2_500], 501.0 / 1_000.0);
        assert_eq!(left[3_500], 500.0 / 1_000.0);
        assert!(biggest_step(&left) <= 1.0 / 1_000.0 + 1e-6);
        let rising: Vec<bool> = heads.windows(2).map(|pair| pair[1] > pair[0]).collect();
        assert_eq!(rising[..7], [true, true, true, false, false, false, false], "the playhead turns at E: {heads:?}");
        assert_eq!(rising[7..11], [true, true, true, true], "and at S: {heads:?}");
    }

    #[test]
    fn levitate_stays_between_the_handles() {
        let tone = loud_sine(4_800, 1_234.0);
        let natural = biggest_step(&tone);
        let ramp_sample = ramp(1_000);
        for (sample, check_steps) in [(ramp_sample, false), (tone, true)] {
            let frames = sample.len();
            let mut engine = Engine::new(RATE);
            engine.settings.direction = Direction::Levitate;
            engine.settings.restart = false;
            engine.settings.start = 0.25;
            engine.settings.end = 0.75;
            engine.set_sample(Some(prepared(vec![sample.clone()])));
            let left = play_block(&mut engine, frames * 4).0;
            if check_steps {
                let step = biggest_step(&left[60..]);
                assert!(step <= natural * 1.001, "turns don't click: {step} against {natural}");
            } else {
                // Past the first frames, which fade in from the middle of the sample.
                let settled = &left[100..];
                let low = settled.iter().copied().fold(f32::MAX, f32::min);
                let high = settled.iter().copied().fold(f32::MIN, f32::max);
                assert_eq!((low, high), (251.0 / 1_000.0, 750.0 / 1_000.0), "it plays S to E and back, over and over");
                let turns = settled.windows(3).filter(|w| (w[1] - w[0]) * (w[2] - w[1]) < 0.0 || w[1] == w[0]).count();
                assert!(turns >= 6, "{turns} turns");
            }
        }
    }

    /// Plays `blocks` blocks of `block` frames from `start_beat` at 120 bpm, the transport moving
    /// on with each, with `gains`. Returns the left output and the playhead after each block.
    fn play_along(engine: &mut Engine, start_beat: f64, block: usize, blocks: usize, gains: FrameGains) -> (Vec<f32>, Vec<Option<f32>>) {
        let step = 120.0 / 60.0 / RATE;
        let (mut out, mut heads) = (Vec::new(), Vec::new());
        for index in 0..blocks {
            let transport = TransportInfo::playing_at(start_beat + (index * block) as f64 * step, 120.0, RATE);
            let mut left = vec![if gains.mix > 0.0 { 1.0 } else { 0.0 }; block];
            let mut right = left.clone();
            let report = engine.process(&mut [&mut left[..], &mut right[..]], &transport, || gains);
            out.extend(left);
            heads.push(report.playhead);
        }
        (out, heads)
    }

    fn levitate_with_restart(end: f32) -> Engine {
        let mut engine = Engine::new(RATE);
        engine.settings.direction = Direction::Levitate;
        assert!(engine.settings.restart);
        engine.settings.end = end;
        engine.set_sample(Some(prepared(vec![ramp(48_000)])));
        engine
    }

    #[test]
    fn levitate_with_restart_alternates_forward_and_backward_each_step() {
        // A quarter note at 120 bpm is 24,000 frames; the region is the first 12,000.
        let mut engine = levitate_with_restart(0.25);
        let (left, heads) = play_along(&mut engine, 0.0, 500, 192, sample_only());
        let declick = (DECLICK_SECONDS * RATE) as usize;
        for step in 0..4 {
            let at = step * 24_000;
            if step % 2 == 0 {
                // Forward from S, from the song's first step, fading out at E (mid-sample).
                for n in 0..12_000 - declick {
                    assert_eq!(left[at + n], (n + 1) as f32 / 48_000.0, "step {step} frame {n}");
                }
            } else {
                // Backward from E, fading in there because it is mid-sample, down to S.
                assert!(left[at] < 12_000.0 / 48_000.0 / declick as f32 + 1e-6, "step {step} fades in");
                for n in declick..12_000 {
                    assert_eq!(left[at + n], (12_000 - n) as f32 / 48_000.0, "step {step} frame {n}");
                }
            }
            // One pass per step: no bounce at the far handle, silence until the next step.
            assert!(left[at + 12_000..at + 24_000].iter().all(|&v| v == 0.0), "step {step} stops at the far handle");
            // The playhead follows: rising on forward steps, falling on backward ones.
            let first = at / 500 + 2;
            let moves: Vec<bool> = (first..first + 20).map(|b| heads[b + 1].unwrap() > heads[b].unwrap()).collect();
            assert!(moves.iter().all(|&up| up == (step % 2 == 0)), "step {step}: {:?}", &heads[first..first + 21]);
        }
    }

    #[test]
    fn levitate_with_restart_goes_by_the_song_position_wherever_playback_starts() {
        let mut whole = levitate_with_restart(0.25);
        let reference = play_along(&mut whole, 0.0, 500, 192, sample_only()).0;
        // Starting on the second, third, or fourth quarter note plays exactly what a run from the
        // song's start plays there: backward, forward, backward.
        for start in 1..4 {
            let mut engine = levitate_with_restart(0.25);
            let left = play_along(&mut engine, start as f64, 500, 48, sample_only()).0;
            let expected = &reference[start * 24_000..start * 24_000 + 24_000];
            let worst = left.iter().zip(expected).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
            assert!(worst < 1e-6, "from beat {start}: off by {worst}");
            let backwards = left[3_000] > left[6_000];
            assert_eq!(backwards, start % 2 == 1, "from beat {start}");
        }
        // Stopping and starting again on the same step goes the same way.
        let mut engine = levitate_with_restart(0.25);
        play_along(&mut engine, 1.0, 500, 10, sample_only());
        play(&mut engine, 500, TransportInfo::stopped());
        let again = play_along(&mut engine, 1.0, 500, 10, sample_only()).0;
        assert!(again[3_000] > again[4_000], "backward again on the second quarter note");
    }

    #[test]
    fn levitate_with_restart_retriggers_without_clicks() {
        // Steps shorter than the region, so every step cuts a pass off mid-way and turns round,
        // on a loud 3 kHz tone, with the region at the file's ends and in the middle of it.
        let tone = loud_sine(48_000, 3_000.0);
        let natural = biggest_step(&tone);
        for (start, end) in [(0.0, 1.0), (0.1, 0.9), (0.237, 0.611)] {
            let mut worst = [0.0f32; 3];
            for (index, direction) in [Direction::Forward, Direction::Backward, Direction::Levitate].into_iter().enumerate() {
                let mut engine = Engine::new(RATE);
                engine.settings.direction = direction;
                engine.settings.rate = SyncRate::Sixteenth;
                engine.settings.start = start;
                engine.settings.end = end;
                engine.set_sample(Some(prepared(vec![tone.clone()])));
                let left = play_along(&mut engine, 0.0, 512, 100, sample_only()).0;
                worst[index] = biggest_step(&left[60..]);
            }
            let [forward, backward, levitate] = worst;
            assert!(levitate <= natural * 1.15, "{start}-{end}: {levitate} against the tone's own {natural}");
            assert!(levitate <= forward.max(backward) * 1.001, "{start}-{end}: no worse than Forward or Backward: {worst:?}");
        }
    }

    #[test]
    fn levitate_with_restart_ducks_with_the_way_it_plays() {
        // A 400 ms kick: played forward its hit is at the start of the step, backward at the end
        // of the pass, and the duck (which also drives Dream Eater's swell) follows.
        let mut engine = Engine::new(RATE);
        engine.settings.direction = Direction::Levitate;
        engine.set_sample(Some(prepared(vec![kick(400.0)])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 };
        let (gain, _) = play_along(&mut engine, 0.0, 500, 144, gains);
        let deepest = |range: std::ops::Range<usize>| {
            range.clone().min_by(|&a, &b| gain[a].total_cmp(&gain[b])).unwrap() - range.start
        };
        let forward = deepest(0..24_000);
        let backward = deepest(24_000..48_000);
        assert!(forward < 2_400, "forward ducks at the hit, {forward} frames in");
        assert!((16_800..19_300).contains(&backward), "backward ducks as the pass reaches the hit, {backward} frames in");
        assert!(gain[48_000 + forward] < 0.2, "and forward again on the third step");
    }

    #[test]
    fn levitate_judges_its_peak_both_ways_with_restart() {
        let mut engine = levitate_with_restart(0.25);
        let key = engine.analysis_key(engine.sample().unwrap());
        assert!(key.fades_out, "each pass fades out at a handle mid-sample, like Forward");
        assert_eq!(key.alternates, Some(false), "and the way back stops at the file's start, needing no fade");
        engine.finish_analysis();
        assert!(engine.analysis.peak().is_some_and(|peak| peak > 0.0));
        engine.settings.restart = false;
        assert_eq!(engine.analysis_key(engine.sample().unwrap()).alternates, None, "without Restart it bounces");
    }

    #[test]
    fn levitate_judges_its_peak_without_fades() {
        let mut engine = Engine::new(RATE);
        engine.settings.direction = Direction::Levitate;
        engine.settings.restart = false;
        engine.set_sample(Some(prepared(vec![ramp(4_800)])));
        let key = engine.analysis_key(engine.sample().unwrap());
        assert!(!key.fades_out, "a bounce never fades");
    }

    #[test]
    fn reverse_loops_backwards_without_restart() {
        let mut engine = Engine::new(RATE);
        engine.settings.direction = Direction::Backward;
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
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 };
        let (left, _) = run(&mut engine, 12_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let lowest = left.iter().cloned().fold(f32::MAX, f32::min);
        assert!(lowest < 1e-4, "{lowest}");
    }

    #[test]
    fn stereo_samples_blend_into_matching_channels_and_mono_tracks_get_the_sum() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![vec![0.4; 100], vec![-0.2; 100]])));
        let gains = FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.5, dream_eater: 0.0, drive: 0.0 };
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
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 1.0, dream_eater: 0.0, drive: 0.0 };
        let (left, _) = run(&mut engine, 1_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        assert!(left.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn analysis_runs_incrementally_on_the_audio_thread() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(2_000.0)])));
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 };
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
        left
    }

    /// Processes `input` on both channels and returns the left output then the right.
    fn process_both(
        engine: &mut Engine,
        input: &[f32],
        transport: TransportInfo,
        gains: impl FnMut() -> FrameGains,
    ) -> Vec<f32> {
        let mut left = input.to_vec();
        let mut right = input.to_vec();
        engine.process(&mut [&mut left[..], &mut right[..]], &transport, gains);
        left.extend(right);
        left
    }

    fn kick_engine() -> Engine {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(300.0)])));
        engine.finish_analysis();
        engine
    }

    fn ducking(dream_eater: f32) -> FrameGains {
        FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, dream_eater, drive: 0.0 }
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

        // Once it is back at Off and the gate has closed, the track matches one that never opened.
        let (mut was_on, mut never_on) = (kick_engine(), kick_engine());
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        process_track(&mut was_on, &input[..24_000], playing, || ducking(1.0));
        process_track(&mut never_on, &input[..24_000], playing, || ducking(0.0));
        let next = TransportInfo::playing_at(1.0, 120.0, RATE);
        let tail = 48_000;
        let back = process_track(&mut was_on, &input[..tail], next, || ducking(0.0));
        let plain = process_track(&mut never_on, &input[..tail], next, || ducking(0.0));
        let settled = 24_000;
        assert_eq!(&back[settled..], &plain[settled..], "Off is bit-exact again once the gate has closed");
    }

    /// Goertzel magnitude of `signal` at `hz`.
    fn tone_level(signal: &[f32], hz: f64) -> f32 {
        let omega = std::f32::consts::TAU * hz as f32 / RATE as f32;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for sample in signal {
            let s0 = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / signal.len() as f32
    }

    /// A loud 440 Hz partial plus a quiet 3 kHz one.
    fn partials(frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| {
                let t = n as f32 / RATE as f32;
                (std::f32::consts::TAU * 440.0 * t).sin() * 0.5 + (std::f32::consts::TAU * 3_000.0 * t).sin() * 0.01
            })
            .collect()
    }

    fn gated(amount: f32, frames: usize) -> (Vec<f32>, u32) {
        let mut engine = Engine::new(RATE);
        let gains = FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: amount, drive: 0.0 };
        let output = process_track(&mut engine, &partials(frames), TransportInfo::stopped(), || gains);
        (output, engine.latency_samples())
    }

    #[test]
    fn dream_eater_keeps_the_loud_partial_and_strips_the_quiet_one() {
        let frames = RATE as usize;
        let (output, latency) = gated(1.0, frames);
        assert!(latency >= 1_024, "an open gate reports its STFT delay, {latency}");
        let body = &output[latency as usize + 8_000..];
        let loud = tone_level(body, 440.0);
        let quiet = tone_level(body, 3_000.0);
        assert!(loud > 0.05, "the loud partial should remain, {loud}");
        assert!(quiet < loud * 0.02, "the partial under the threshold should be stripped, quiet {quiet} loud {loud}");
    }

    #[test]
    fn turning_dream_eater_up_thins_the_quiet_partial_more() {
        let frames = RATE as usize;
        let ratio = |amount: f32| {
            let (output, latency) = gated(amount, frames);
            let body = &output[latency as usize + 8_000..];
            tone_level(body, 3_000.0) / tone_level(body, 440.0)
        };
        let (light, full) = (ratio(0.3), ratio(1.0));
        assert!(light > 0.01, "at 30% the quiet partial is still mostly there, {light}");
        assert!(full < light * 0.25, "100% strips it further: {full} vs {light}");
    }

    #[test]
    fn dream_eater_hears_the_track_after_the_duck() {
        let frames = 2 * RATE as usize;
        let input = sine(440.0, frames);
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let with_duck = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0, drive: 0.0 };
        let no_duck = FrameGains { depth: 0.0, ..with_duck };
        let mut ducked = kick_engine();
        let holed = process_track(&mut ducked, &input, playing, || with_duck);
        let open = process_track(&mut kick_engine(), &input, playing, || no_duck);
        let latency = ducked.latency_samples() as usize;
        let (gain, _) = run(&mut kick_engine(), frames, playing, FrameGains { dream_eater: 0.0, ..with_duck });
        let bottom = (0..frames).min_by(|&a, &b| gain[a].total_cmp(&gain[b])).unwrap();
        assert!(gain[bottom] < 0.15, "the kick ducks the tone: {}", gain[bottom]);
        let at = bottom + latency;
        let window = 480;
        assert!(at + window < frames, "the delayed hole is still inside the render");
        let hole = rms(&holed[at..at + window]);
        let tone = rms(&open[at..at + window]);
        assert!(tone > 0.05, "without the duck the tone is still there, {tone}");
        assert!(hole < tone * 0.35, "the ducked tone stays ducked after the gate: {hole} vs {tone}");
    }

    /// A lone sine is kept whole by the gate, so ducking it must match Dream Eater off, one frame later.
    /// Gating the already-ducked tone would smear the dip across the spectral window.
    #[test]
    fn the_duck_stays_sharp_on_a_gated_sine() {
        let frames = 2 * RATE as usize;
        let input = sine(440.0, frames);
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive: 0.0 };
        let plain = process_track(&mut kick_engine(), &input, playing, || gains);
        let mut gated_engine = kick_engine();
        let gated = process_track(&mut gated_engine, &input, playing, || FrameGains { dream_eater: 1.0, ..gains });
        let latency = gated_engine.latency_samples() as usize;
        let mut err = 0.0f32;
        let mut energy = 0.0f32;
        for i in latency + 8_000..frames {
            let diff = gated[i] - plain[i - latency];
            err += diff * diff;
            energy += plain[i - latency] * plain[i - latency];
        }
        let ratio = err / energy.max(1.0e-12);
        assert!(ratio < 1.0e-3, "the duck on a gated sine matches the unducked path: error {ratio}");
    }

    #[test]
    fn the_blended_sample_is_not_thinned() {
        let frames = RATE as usize;
        let silence = vec![0.0f32; frames];
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let gains = |amount: f32| FrameGains { depth: 0.0, mix: 0.0, sample_level: 1.0, dream_eater: amount, drive: 0.0 };
        let mut on = kick_engine();
        let wet = process_track(&mut on, &silence, playing, || gains(1.0));
        let dry = process_track(&mut kick_engine(), &silence, playing, || gains(0.0));
        let latency = on.latency_samples() as usize;
        assert!(latency > 0);
        let mut worst = 0.0f32;
        for i in latency..frames {
            worst = worst.max((wet[i] - dry[i - latency]).abs());
        }
        assert!(worst < 1.0e-4, "the sample is delayed with the gate, not splattered: {worst}");
        assert!(rms(&dry) > 0.01, "the kick is actually in the dry render");
    }

    #[test]
    fn filters_and_drive_leave_the_gated_track_alone() {
        let input = partials(RATE as usize);
        let playing = TransportInfo::stopped();
        let run = |high: f32, low: f32, drive: f32| {
            let mut engine = kick_engine();
            engine.settings.high_pass_hz = high;
            engine.settings.low_pass_hz = low;
            let gains = FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0, drive };
            process_track(&mut engine, &input, playing, || gains)
        };
        let open = run(HP_OFF_HZ, LP_OFF_HZ, 0.0);
        assert_eq!(run(2_000.0, 50.0, 0.0), open, "HP and LP filter the sample, not the ducked track");
        assert_eq!(run(HP_OFF_HZ, LP_OFF_HZ, 1.0), open, "Drive saturates the blended sample, not the gate");
    }

    #[test]
    fn dream_eater_at_full_adds_no_hiss_to_silence_and_stays_bounded() {
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let silence = vec![0.0f32; RATE as usize];
        let output = process_track(&mut kick_engine(), &silence, playing, || ducking(1.0));
        let peak = output.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 1.0e-5, "silence in is silence out: {peak}");

        let mut engine = Engine::new(RATE);
        let noise = noise(RATE as usize);
        let gains = FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0, drive: 0.0 };
        let loud = process_track(&mut engine, &noise, TransportInfo::stopped(), || gains);
        assert!(loud.iter().all(|v| v.is_finite()));
        let peak = loud.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 1.5, "full-scale noise stays bounded: {peak}");
    }

    #[test]
    fn a_mono_track_gets_the_same_gate_as_a_centred_stereo_one() {
        let input = partials(RATE as usize);
        let gains = FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0, drive: 0.0 };
        let mut mono = input.clone();
        Engine::new(RATE).process(&mut [&mut mono[..]], &TransportInfo::stopped(), || gains);
        let stereo = process_track(&mut Engine::new(RATE), &input, TransportInfo::stopped(), || gains);
        let worst = mono.iter().zip(&stereo).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(worst < 1.0e-5, "mono matches the centred stereo left: {worst}");
    }

    /// FNV-1a over the bits of `samples`.
    fn fingerprint(samples: &[f32]) -> u64 {
        samples.iter().fold(0xcbf2_9ce4_8422_2325, |hash, v| {
            v.to_bits().to_le_bytes().iter().fold(hash, |hash, &byte| (hash ^ byte as u64).wrapping_mul(0x100_0000_01b3))
        })
    }

    /// Water's kick ducking `track` at 120 bpm, with HP at 150 Hz and LP at 9 kHz, both sides.
    fn drive_scene(engine: &mut Engine, gains: impl Fn() -> FrameGains + Copy) -> Vec<f32> {
        let input = track(3 * RATE as usize);
        engine.settings.high_pass_hz = 150.0;
        engine.settings.low_pass_hz = 9_000.0;
        let mut output = Vec::new();
        for (block, chunk) in input.chunks(512).enumerate() {
            let transport = TransportInfo::playing_at((block * 512) as f64 * 2.0 / RATE, 120.0, RATE);
            output.extend(process_both(engine, chunk, transport, gains));
        }
        output
    }

    fn blended(drive: f32) -> FrameGains {
        FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.5, dream_eater: 0.6, drive }
    }

    #[test]
    fn drive_at_0_is_deterministic() {
        let gains = FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.5, dream_eater: 0.0, drive: 0.0 };
        let once = drive_scene(&mut kick_engine(), || gains);
        let twice = drive_scene(&mut kick_engine(), || gains);
        assert_eq!(once, twice, "Drive at 0 does not depend on anything but the duck and the sample");
        assert!(once.iter().any(|sample| sample.abs() > 0.1));
    }

    /// The track gain over a 400 ms kick ducking a constant track by 75%.
    fn dip(drive: f32) -> Vec<f32> {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(400.0)])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, dream_eater: 0.0, drive };
        run(&mut engine, 24_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains).0
    }

    fn frames_below(gain: &[f32], level: f32) -> usize {
        gain.iter().filter(|&&v| v < level).count()
    }

    #[test]
    fn drive_thickens_the_dips_without_deepening_them() {
        let clean = dip(0.0);
        let mut last = clean.clone();
        for drive in [0.5, 1.0] {
            let driven = dip(drive);
            let lowest = |gain: &[f32]| gain.iter().cloned().fold(f32::MAX, f32::min);
            assert!((lowest(&driven) - lowest(&clean)).abs() < 1e-4, "{drive}: {} vs {}", lowest(&driven), lowest(&clean));
            for level in [0.5, 0.75, 0.9] {
                assert!(frames_below(&driven, level) > frames_below(&last, level), "{drive} at {level}");
            }
            assert!(driven.iter().zip(&clean).all(|(d, c)| *d <= *c + 1e-6), "{drive}: the bend only deepens");
            last = driven;
        }
    }

    /// The blended sample alone at `drive`, over four seconds at 120 bpm, so it plays every beat.
    fn sample_alone(sample: &Arc<PreparedSample>, drive: f32) -> Vec<f32> {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(sample.clone()));
        engine.finish_analysis();
        let gains = FrameGains { depth: 0.0, mix: 0.0, sample_level: 1.0, dream_eater: 0.0, drive };
        let mut out = Vec::new();
        for start in (0..4 * RATE as usize).step_by(512) {
            let transport = TransportInfo::playing_at(start as f64 * 2.0 / RATE, 120.0, RATE);
            out.extend(process_constant(&mut engine, 512, transport, gains, 0.0).0);
        }
        out
    }

    /// A plucked tone: `hz` at half scale, dying away four times over a second, `ms` long.
    fn pluck(hz: f64, ms: f64) -> Vec<f32> {
        let frames = (ms * 0.001 * RATE) as usize;
        sine(hz, frames).iter().enumerate().map(|(n, v)| v * 0.5 * (-4.0 * n as f64 / RATE).exp() as f32).collect()
    }

    /// The level of `hz` in `x`, in dB.
    fn level_at(x: &[f32], hz: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * hz / RATE;
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s = v as f64 + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        10.0 * (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).max(1e-300).log10()
    }

    #[test]
    fn drive_brings_the_samples_body_up_under_its_peaks() {
        let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        // Water's peaks and the kick's are their sub, which the ceiling holds, so their body comes
        // up less, and stops coming up once the sub is at the ceiling.
        let factory = ["Daisy", "Paper", "Press", "Spin", "Stone", "Umber", "Water", "Wind"].map(|name| {
            let sample = crate::preset::Preset::factory(name).unwrap().sample.unwrap();
            (name, Arc::new(PreparedSample::new(&sample, RATE)), name != "Water")
        });
        let made = [("kick", prepared(vec![kick(300.0)])), ("pluck", prepared(vec![pluck(1_000.0, 500.0)]))];
        for (name, sample, lifted) in factory.into_iter().chain(made.map(|(name, sample)| (name, sample, false))) {
            let clean = sample_alone(&sample, 0.0);
            let settled = RATE as usize;
            let mut last = -1.0;
            for drive in [0.25, 0.5, 0.75, 1.0] {
                let driven = sample_alone(&sample, drive);
                let db = 20.0 * (rms(&driven[settled..]) / rms(&clean[settled..])).log10();
                // Coming up with the knob, a little at the middle and about 3 dB at the top.
                if name == "Water" || name == "kick" {
                    assert!(db > -0.5, "{name} at {drive}: {db} dB");
                } else {
                    assert!(db > last, "{name} at {drive}: {db} dB after {last}");
                }
                if lifted && drive == 0.5 {
                    assert!((0.3..2.5).contains(&db), "{name} at {drive}: {db} dB");
                }
                if lifted && drive == 1.0 {
                    assert!((2.0..4.0).contains(&db), "{name} at {drive}: {db} dB");
                }
                last = db;
                // And never past the clean sample's peak (within 0.01 dB).
                assert!(peak(&driven) <= 1.001 * peak(&clean), "{name} at {drive}: {} vs {}", peak(&driven), peak(&clean));
            }
        }
    }

    #[test]
    fn drive_leads_with_the_second_harmonic_at_1_khz() {
        let sample = prepared(vec![pluck(1_000.0, 500.0)]);
        let mut last = -100.0;
        for drive in [0.25, 0.5, 0.75, 1.0] {
            let out = sample_alone(&sample, drive);
            let out = &out[RATE as usize..];
            let fundamental = level_at(out, 1_000.0);
            let (second, third) = (level_at(out, 2_000.0) - fundamental, level_at(out, 3_000.0) - fundamental);
            assert!(second > third + 3.0, "{drive}: H2 {second} dB, H3 {third} dB");
            assert!(second > last, "{drive}: H2 {second} dB after {last}");
            last = second;
        }
    }

    #[test]
    fn drive_warms_the_sample_and_the_chord_and_holds_the_peaks() {
        let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let clean = drive_scene(&mut kick_engine(), || blended(0.0));
        for drive in [0.5, 1.0] {
            let driven = drive_scene(&mut kick_engine(), || blended(drive));
            let change: Vec<f32> = driven.iter().zip(&clean).map(|(a, b)| a - b).collect();
            assert!(rms(&change) > 0.01 * rms(&clean), "{drive}: it does something");
            let db = 20.0 * (rms(&driven) / rms(&clean)).log10();
            assert!(db > -0.5 && db < 3.0, "{drive}: {db} dB");
            assert!(peak(&driven) <= peak(&clean) * 1.02, "{drive}: {} vs {}", peak(&driven), peak(&clean));
        }

        // No blended sample: Drive still bends the duck the gate hears, and the peaks hold.
        let gated = |drive: f32| move || FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, dream_eater: 1.0, drive };
        let (clean, driven) = (drive_scene(&mut kick_engine(), gated(0.0)), drive_scene(&mut kick_engine(), gated(1.0)));
        assert_ne!(fingerprint(&clean), fingerprint(&driven), "the bent duck still shapes the gated track");
        assert!(peak(&driven) <= peak(&clean) * 1.05 + 0.02, "gated: {} vs {}", peak(&driven), peak(&clean));
    }

    #[test]
    fn turning_drive_does_not_click() {
        let held = (0..=10)
            .map(|step| {
                let drive = step as f32 / 10.0;
                let mut engine = kick_engine();
                let gains = FrameGains { depth: 0.0, mix: 0.0, sample_level: 1.0, dream_eater: 0.0, drive };
                biggest_step(&process_constant(&mut engine, 4 * RATE as usize, TransportInfo::playing_at(0.0, 120.0, RATE), gains, 0.0).0)
            })
            .fold(0.0, f32::max);
        // Up and down the knob through 0, at the rate a host would ramp it, in blocks.
        let mut engine = kick_engine();
        let mut out = Vec::new();
        let frames = 4 * RATE as usize;
        for start in (0..frames).step_by(256) {
            let t = start as f32 / frames as f32;
            let drive = ((1.0 - (2.0 * t - 1.0).abs()) * 1.2 - 0.1).clamp(0.0, 1.0);
            let gains = FrameGains { depth: 0.0, mix: 0.0, sample_level: 1.0, dream_eater: 0.0, drive };
            let transport = TransportInfo::playing_at(start as f64 * 2.0 / RATE, 120.0, RATE);
            out.extend(process_constant(&mut engine, 256, transport, gains, 0.0).0);
        }
        // The steepest moments are the kick's own attack, a little louder while the loudness match
        // catches up with the knob. A click would jump far beyond them.
        let biggest = biggest_step(&out);
        assert!(biggest < held * 1.15, "{biggest} against {held} held");
    }

    /// Prints what Drive does to the dips, and what it costs. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn drive_report() {
        let clean = dip(0.0);
        for drive in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let gain = dip(drive);
            let ms = |frames: usize| frames as f64 * 1000.0 / RATE;
            println!(
                "drive {drive:.2}: deepest {:.4}, under 50% {:.1} ms, under 75% {:.1} ms, under 90% {:.1} ms (clean {:.1} / {:.1} / {:.1})",
                gain.iter().cloned().fold(f32::MAX, f32::min),
                ms(frames_below(&gain, 0.5)),
                ms(frames_below(&gain, 0.75)),
                ms(frames_below(&gain, 0.9)),
                ms(frames_below(&clean, 0.5)),
                ms(frames_below(&clean, 0.75)),
                ms(frames_below(&clean, 0.9)),
            );
        }
        let clean = drive_scene(&mut kick_engine(), || blended(0.0));
        for drive in [0.25, 0.5, 0.75, 1.0] {
            let driven = drive_scene(&mut kick_engine(), || blended(drive));
            let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            println!(
                "scene at drive {drive:.2}: level {:+.2} dB, peak {:.3} (clean {:.3})",
                20.0 * (rms(&driven) / rms(&clean)).log10(),
                peak(&driven),
                peak(&clean)
            );
        }
        let seconds = 20;
        let input = track(seconds * RATE as usize);
        for drive in [0.0, 1.0] {
            let mut engine = kick_engine();
            let started = std::time::Instant::now();
            for (block, chunk) in input.chunks(512).enumerate() {
                let transport = TransportInfo::playing_at((block * 512) as f64 * 2.0 / RATE, 120.0, RATE);
                process_both(&mut engine, chunk, transport, || blended(drive));
            }
            let spent = started.elapsed().as_secs_f64();
            println!("drive {drive:.0}: {:.2} ms per second of stereo audio ({:.2}% of one core)", spent * 1000.0 / seconds as f64, spent * 100.0 / seconds as f64);
        }
    }
}