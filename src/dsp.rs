//! Haunter's audio engine. Nothing here allocates while processing.

use nih_plug::prelude::Enum;
use std::sync::Arc;

use crate::dream::{DreamEater, KeyChoice};
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
}

impl GridClock {
    pub fn reset(&mut self) {
        self.last_line = None;
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
    /// The key Dream Eater tunes to.
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
    /// Sample volume times sample blend.
    pub sample_level: f32,
    /// 0..1, how strongly the sample plays Dream Eater's resonances on the track. 0 bypasses them.
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
    /// 0..1, how far the sample is currently opening Dream Eater's resonances. 0 while it is off.
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
    /// Dream Eater's resonances on the track, and the key detection that tunes them.
    dream: DreamEater,
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
            dream: DreamEater::new(sample_rate),
        };
        engine.prepare(sample_rate);
        engine
    }

    /// Sets the sample rate and allocates for it. Not for the audio thread.
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.declick = ((DECLICK_SECONDS * sample_rate).round() as usize).max(1);
        self.dream.prepare(sample_rate);
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
        self.dream.reset();
    }

    /// The key Dream Eater is tuned to, C = 0.
    pub fn dream_key(&self) -> u8 {
        self.dream.key()
    }

    /// The key heard in the track, once one has been heard with confidence.
    pub fn detected_key(&self) -> Option<u8> {
        self.dream.detected_key()
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
        self.dream.choose(settings.dream_key);
        self.dream.set_attack(settings.attack_ms);
        if let Some(sample) = self.sample.clone() {
            self.update_region(sample.frames());
            let key = self.analysis_key(&sample);
            self.analysis.step(&sample, key, self.sample_rate, ANALYSIS_BUDGET);
        }
    }

    /// Ducks `channels` in place, adds Dream Eater's chord after the duck, and mixes in the
    /// sample. `gains` is called once per frame.
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
            // The grid line that fired, if one did. Levitate with Restart on goes by its parity.
            let (trigger, line) = match grid {
                Some((beat, step)) => match self.clock.tick_line(beat + frame as f64 * step, step, interval) {
                    Some(line) => (true, Some(line)),
                    None => (false, None),
                },
                None => (std::mem::take(&mut start_loop), None),
            };
            let gains = gains();
            let (track_gain, add_left, add_right, envelope) = self.frame(trigger, line, transport.playing, &gains);
            min_track_gain = min_track_gain.min(track_gain);
            let mut track = [0.0f32; 2];
            for (index, channel) in channels.iter().take(2).enumerate() {
                track[index] = channel[frame];
            }
            self.dream.set_driven(self.sample.is_some());
            let eaten = self.dream.process(track, channel_count.min(2), envelope, track_gain, gains.dream_eater);
            for (index, channel) in channels.iter_mut().enumerate() {
                let add = match (channel_count, index) {
                    (1, _) => 0.5 * (add_left + add_right),
                    (_, 0) => add_left,
                    (_, 1) => add_right,
                    _ => 0.0,
                };
                channel[frame] = match eaten {
                    Some(eaten) if index < 2 => eaten[index] + add,
                    _ => channel[frame] * track_gain + add,
                };
            }
        }

        let playhead = match (&self.sample, &self.voice) {
            (Some(sample), Some(voice)) if sample.frames() > 0 => Some(voice.pos as f32 / sample.frames() as f32),
            _ => None,
        };
        BlockReport { min_track_gain, playhead, dream_level: self.dream.level() }
    }

    fn fade_gain(&self, voice: &Voice, frames: usize, playing: bool) -> f32 {
        voice.declick_gain(self.region, self.fades_out(voice.reverse, frames, playing), self.declick)
    }

    /// Returns (track gain, sample left, sample right, normalised envelope) for one frame.
    fn frame(&mut self, trigger: bool, line: Option<i64>, playing: bool, gains: &FrameGains) -> (f32, f32, f32, f32) {
        let Some(sample) = self.sample.clone() else {
            return (1.0, 0.0, 0.0, 0.0);
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
        engine.settings.direction = Direction::Backward;
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
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, dream_eater: 0.0 };
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

    /// Plays `input` on both channels in 512-frame blocks from beat 0 at 120 bpm with Dream Eater
    /// at `amount`, no ducking and no blended sample, calling `between` before each block with
    /// the frame it starts at. Returns the left output.
    fn play_dream(engine: &mut Engine, input: &[f32], amount: f32, mut between: impl FnMut(&mut Engine, usize)) -> Vec<f32> {
        let gains = FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: amount };
        let mut out = Vec::with_capacity(input.len());
        for (block, chunk) in input.chunks(512).enumerate() {
            between(engine, block * 512);
            let transport = TransportInfo::playing_at((block * 512) as f64 * 2.0 / RATE, 120.0, RATE);
            out.extend(process_track(engine, chunk, transport, || gains));
        }
        out
    }

    /// The same partials as `track`, without the noise, so a click stands out.
    fn smooth_track(frames: usize) -> Vec<f32> {
        let partials = [sine(110.0, frames), sine(233.0, frames), sine(587.0, frames)];
        (0..frames).map(|n| 0.15 * partials.iter().map(|p| p[n]).sum::<f32>()).collect()
    }

    #[test]
    fn without_a_sample_dream_eater_is_bit_exact_off() {
        // There is nothing ducking the track, so Dream Eater at 100% leaves it exactly as Off
        // does, from a fresh instance's first frame, playing or stopped. (1.7.7 rang a standing
        // chord here and turned the track down.)
        let input = track(4 * RATE as usize);
        for transport in [TransportInfo::playing_at(0.0, 120.0, RATE), TransportInfo::stopped()] {
            let mut on = Engine::new(RATE);
            let mut off = Engine::new(RATE);
            let (mut eaten, mut plain) = (Vec::new(), Vec::new());
            for chunk in input.chunks(512) {
                eaten.extend(process_both(&mut on, chunk, transport, || ducking(1.0)));
                plain.extend(process_both(&mut off, chunk, transport, || ducking(0.0)));
            }
            let difference: Vec<f32> = eaten.iter().zip(&plain).map(|(a, b)| a - b).collect();
            assert!(eaten == plain, "playing {}: differs from Off by {} of the track's RMS", transport.playing, rms(&difference) / rms(&plain));
        }
    }

    #[test]
    fn a_loaded_sample_keeps_dream_eater_on_even_while_stopped() {
        // As in 1.7.7: with a sample loaded, the chord rings between hits and with the transport
        // stopped, from the 10% the voices stay open.
        let input = track(2 * RATE as usize);
        let full = || FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0 };
        let output = process_track(&mut kick_engine(), &input, TransportInfo::stopped(), full);
        let change: Vec<f32> = output.iter().zip(&input).map(|(a, b)| a - b).collect();
        assert!(rms(&change[RATE as usize..]) > 0.1 * rms(&input), "{}", rms(&change) / rms(&input));
    }

    #[test]
    fn unloading_the_sample_lets_the_chord_ring_out_and_brings_the_track_back() {
        let input = smooth_track(8 * RATE as usize);
        // On a block boundary, about 2 s in.
        let unload = 188 * 512;
        let mut engine = kick_engine();
        let output = play_dream(&mut engine, &input, 1.0, |engine, at| {
            if at == unload {
                engine.set_sample(None);
            }
        });
        let change: Vec<f32> = output.iter().zip(&input).map(|(a, b)| a - b).collect();

        let before = biggest_step(&change[unload - 24_000..unload]);
        let across = biggest_step(&change[unload - 2_400..unload + 24_000]);
        assert!(across <= before * 1.2, "no click as the sample goes: {across} against {before} before");
        let ringing = rms(&change[unload + 2_400..unload + 7_200]);
        assert!(ringing > 0.1 * rms(&change[unload - 24_000..unload]), "the chord rings on: {ringing}");
        // The track's dip is back to unity within about 30 ms, and the chord has died away and
        // Dream Eater stepped aside, bit-exact, well before 6 s.
        let later = rms(&change[unload + 2 * RATE as usize..unload + 3 * RATE as usize]);
        assert!(later < 1e-3 * rms(&input), "{later}");
        assert!(output[6 * RATE as usize..] == input[6 * RATE as usize..], "bit-exact once it has rung out");
    }

    #[test]
    fn loading_a_sample_brings_dream_eater_in_without_a_click() {
        let input = smooth_track(6 * RATE as usize);
        // Half a block after the block boundary where it is loaded, about 2 s in.
        let load = 188 * 512 + 256;
        let mut engine = Engine::new(RATE);
        let output = play_dream(&mut engine, &input, 1.0, |engine, at| {
            if at == load - 256 {
                engine.set_sample(Some(prepared(vec![kick(300.0)])));
                engine.finish_analysis();
            }
        });
        assert!(output[..load - 256] == input[..load - 256], "bit-exact before the sample is loaded");
        let change: Vec<f32> = output.iter().zip(&input).map(|(a, b)| a - b).collect();
        let arriving = biggest_step(&change[load - 2_400..load + 4_800]);
        let settled = biggest_step(&change[load + 48_000..load + 96_000]);
        assert!(arriving <= settled * 1.2, "the dip comes in smoothly: {arriving} against {settled} later");
        assert!(rms(&change[load + 48_000..]) > 0.1 * rms(&input), "and Dream Eater is on");
    }

    #[test]
    fn turning_dream_eater_up_changes_the_track_more() {
        let input = track(96_000);
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let dry = process_both(&mut kick_engine(), &input, playing, || ducking(0.0));
        let change = |amount: f32| {
            let eaten = process_both(&mut kick_engine(), &input, playing, || ducking(amount));
            let difference: Vec<f32> = eaten.iter().zip(&dry).map(|(a, b)| a - b).collect();
            rms(&difference) / rms(&dry)
        };
        let (light, half, full) = (change(0.1), change(0.5), change(1.0));
        assert!(light > 0.02, "even a little is audible: {light}");
        assert!(half > light * 1.5 && full > half * 1.2, "{light} < {half} < {full}");
    }

    #[test]
    fn the_sample_swells_the_resonance_in_time_with_its_hits() {
        // No ducking, so the track only changes where Dream Eater adds to it.
        let input = track(192_000);
        let full = || FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0 };
        let mut kicked = kick_engine();
        let (mut swelled, mut levels) = ([Vec::new(), Vec::new()], Vec::new());
        for chunk in 0..(input.len() / 2_400) {
            let range = chunk * 2_400..(chunk + 1) * 2_400;
            let transport = TransportInfo::playing_at(range.start as f64 * 2.0 / RATE, 120.0, RATE);
            let mut left = input[range.clone()].to_vec();
            let mut right = left.clone();
            let report = kicked.process(&mut [&mut left[..], &mut right[..]], &transport, full);
            swelled[0].extend(left);
            swelled[1].extend(right);
            levels.push(report.dream_level);
        }
        // Over the last four beats, once the voices and the leveller have settled.
        let moved = |from: usize, to: usize| {
            let (mut during, mut steady) = (Vec::new(), Vec::new());
            for beat in 4..8 {
                let range = beat * 24_000 + from..beat * 24_000 + to;
                for side in &swelled {
                    during.extend(side[range.clone()].iter().zip(&input[range.clone()]).map(|(a, b)| a - b));
                    steady.extend_from_slice(&input[range.clone()]);
                }
            }
            rms(&during) / rms(&steady)
        };
        // Since 1.7.5 the chord blooms in after the duck and peaks 150 to 200 ms after the hit.
        let on_the_hit = moved(4_800, 12_000);
        let between_hits = moved(21_600, 24_000);
        assert!(on_the_hit > 0.15, "the kick swells the resonance: {on_the_hit}");
        assert!(between_hits < on_the_hit * 0.7, "and it rings out after: {between_hits} vs {on_the_hit}");
        // The swell blooms over about a hundred milliseconds and lets go over a few hundred, so
        // by the end of the beat it has fallen to under a fifth of its peak.
        assert!(levels[1] > 0.3 && levels[9] < levels[2] * 0.2, "the editor sees the swell: {levels:?}");
    }

    #[test]
    fn the_resonance_is_tuned_to_the_key_and_swells_without_a_pitch_sweep() {
        // A sustained mix of noise; every hit should ring on the same pitches, so the wet's
        // spectrum at C, G, and D stays put while the hit comes and goes.
        let input: Vec<f32> = noise(96_000).iter().map(|v| v * 0.3).collect();
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let full = || FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0 };
        let mut engine = kick_engine();
        engine.settings.dream_key = KeyChoice::Fixed(0);
        let output = process_track(&mut engine, &input, playing, full);
        // The track's share of the output, a little above `dry_gain` where the chord finds little
        // in noise to ring and the makeup brings the loudness back, found beat by beat. The chord
        // hears the track tens of milliseconds late, so it has nothing in common with it.
        let mut wet = Vec::with_capacity(output.len());
        for (out, track) in output.chunks(24_000).zip(input.chunks(24_000)) {
            let dot: f32 = out.iter().zip(track).map(|(a, b)| a * b).sum();
            let share = dot / track.iter().map(|v| v * v).sum::<f32>();
            wet.extend(out.iter().zip(track).map(|(a, b)| a - share * b));
        }
        let power = |signal: &[f32], hz: f64| {
            let coefficient = 2.0 * (2.0 * std::f64::consts::PI * hz / RATE).cos();
            let (mut s1, mut s2) = (0.0, 0.0);
            for &v in signal {
                let s0 = v as f64 + coefficient * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            (s1 * s1 + s2 * s2 - coefficient * s1 * s2) / (signal.len() as f64).powi(2)
        };
        let root = crate::dream::root_hz(0);
        let mean_power = |window: &[f32], semitones: &[f64]| {
            semitones.iter().map(|s| power(window, root * (s / 12.0).exp2())).sum::<f64>() / semitones.len() as f64
        };
        // One beat at a time, each with its hit and the ring after it.
        for beat in 1..4 {
            let window = &wet[beat * 24_000..(beat + 1) * 24_000];
            let chord = mean_power(window, &[0.0, 7.0, 14.0]);
            let between = mean_power(window, &[1.0, 3.0, 5.0, 6.0, 8.0, 10.0]);
            assert!(chord > 20.0 * between, "beat {beat} rings on the chord: {chord} vs {between}");
        }
        assert_eq!(engine.dream_key(), 0);
    }

    #[test]
    fn dream_eater_at_full_adds_no_hiss_to_silence() {
        // A silent track, ducked on every hit, at 100%: nothing comes out but silence.
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let mut engine = kick_engine();
        let silence = vec![0.0f32; 4 * RATE as usize];
        let mut levels = Vec::new();
        let mut output = Vec::new();
        for (block, chunk) in silence.chunks(2_400).enumerate() {
            let transport = TransportInfo::playing_at((block * 2_400) as f64 * 2.0 / RATE, 120.0, RATE);
            let mut left = chunk.to_vec();
            let mut right = chunk.to_vec();
            let report = engine.process(&mut [&mut left[..], &mut right[..]], &transport, || ducking(1.0));
            levels.push(report.dream_level);
            output.extend(left);
            output.extend(right);
        }
        assert!(levels.iter().any(|&level| level > 0.5), "the hits opened the voices: {levels:?}");
        assert!(output.iter().all(|&v| v == 0.0), "no hiss: peak {}", output.iter().fold(0.0f32, |m, v| m.max(v.abs())));

        // And once a track stops, the chord rings out to true silence rather than a noise floor.
        let input = track(96_000);
        process_track(&mut engine, &input, playing, || ducking(1.0));
        let tail = process_track(&mut engine, &silence, TransportInfo::playing_at(4.0, 120.0, RATE), || ducking(1.0));
        assert!(tail[tail.len() - 24_000..].iter().all(|v| v.abs() < 1e-6), "the tail dies away to silence");
    }

    #[test]
    fn dream_eater_follows_the_tracks_key_even_while_off() {
        let d_major: &[&[u8]] = &[&[38, 50, 54, 57], &[45, 57, 61, 64], &[47, 54, 59, 62], &[43, 55, 59, 62]];
        let frames = 8 * RATE as usize;
        let input = chord_track(d_major, frames);
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let mut engine = kick_engine();
        assert_eq!(engine.dream_key(), crate::dream::FALLBACK_KEY, "C until a key is heard");
        let output = process_track(&mut engine, &input, playing, || ducking(0.0));
        let (gain, _) = run(&mut kick_engine(), frames, playing, ducking(0.0));
        assert!(output.iter().zip(&input).zip(&gain).all(|((o, i), g)| *o == i * g), "Off is still bit-exact");
        assert_eq!(engine.detected_key(), Some(2));
        assert_eq!(engine.dream_key(), 2, "ready in D the moment Dream Eater is turned up");
        engine.settings.dream_key = KeyChoice::Fixed(9);
        process_track(&mut engine, &input[..512], playing, || ducking(0.0));
        assert_eq!(engine.dream_key(), 9, "a chosen key wins over the detected one");
    }

    /// Worst-case material at full Dream Eater: full-scale noise, then a full-scale tone on the
    /// resonance's root, retriggered every 1/16 with the fastest attack. Returns the engine and
    /// the output's peak.
    fn worst_case() -> (Engine, f32) {
        let mut engine = Engine::new(RATE);
        engine.settings.rate = SyncRate::Sixteenth;
        engine.settings.attack_ms = 0.1;
        engine.settings.release_ms = 5.0;
        engine.set_sample(Some(prepared(vec![kick(300.0)])));
        engine.finish_analysis();
        let full = || FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0 };
        let frames = 4 * RATE as usize;
        let mut input = noise(frames / 2);
        input.extend(sine(crate::dream::root_hz(engine.dream_key()), frames / 2));
        let mut peak = 0.0f32;
        let mut sum = 0.0f64;
        for (block, chunk) in input.chunks(512).enumerate() {
            let transport = TransportInfo::playing_at((block * 512) as f64 * 2.0 / RATE, 120.0, RATE);
            let out = process_track(&mut engine, chunk, transport, full);
            assert!(out.iter().all(|v| v.is_finite()), "block {block}");
            peak = out.iter().fold(peak, |m, v| m.max(v.abs()));
            sum += out.iter().map(|&v| v as f64).sum::<f64>();
        }
        assert!((sum / frames as f64).abs() < 1e-3, "no DC: {}", sum / frames as f64);
        (engine, peak)
    }

    #[test]
    fn dream_eater_at_full_stays_finite_bounded_and_rings_out() {
        let (mut engine, peak) = worst_case();
        assert!(peak <= 1.02, "peaks stay at full scale on full-scale material: {peak}");

        let full = || FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.0, dream_eater: 1.0 };
        let tail = process_track(&mut engine, &vec![0.0; 4 * RATE as usize], TransportInfo::stopped(), full);
        assert!(tail[..2_400].iter().any(|v| v.abs() > 1e-4), "it rings on after the track stops");
        assert!(tail[tail.len() - 2_400..].iter().all(|v| v.abs() < 1e-6), "and dies away");
    }

    #[test]
    fn dream_eater_keeps_quiet_tracks_quiet_and_mono_tracks_mono() {
        let input: Vec<f32> = track(48_000).iter().map(|v| v * 0.1).collect();
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let mut engine = kick_engine();
        let mut mono = input.clone();
        engine.process(&mut [&mut mono[..]], &playing, || ducking(1.0));
        assert!(mono.iter().all(|v| v.is_finite()));
        let change: Vec<f32> = mono.iter().zip(&input).map(|(a, b)| a - b).collect();
        assert!(rms(&change) > 0.02 * rms(&input), "a mono track gets the effect too");
        assert!(mono.iter().fold(0.0f32, |m, v| m.max(v.abs())) < 0.2, "and no louder than its scale suggests");
    }

    fn band_rms(signal: &[f32], cutoff: f64) -> f32 {
        let mut filter = Svf::default();
        filter.set(cutoff, RATE);
        let high: Vec<f32> = signal.iter().map(|&v| filter.process(v as f64).1 as f32).collect();
        rms(&high)
    }

    /// Plucked chords, given as MIDI notes, half a second each, round and round.
    fn chord_track(chords: &[&[u8]], frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| {
                let chord = chords[(n / 24_000) % chords.len()];
                let t = (n % 24_000) as f64 / RATE;
                let pluck = (1.0 - (-t * 200.0).exp()) * (-t * 2.0).exp();
                let sum: f64 = chord
                    .iter()
                    .map(|&note| {
                        let hz = 440.0 * ((note as f64 - 69.0) / 12.0).exp2();
                        (1..=3).map(|h| (2.0 * std::f64::consts::PI * hz * h as f64 * n as f64 / RATE).sin() / h as f64).sum::<f64>()
                    })
                    .sum();
                (0.08 * pluck * sum) as f32
            })
            .collect()
    }

    const A_MINOR: &[&[u8]] = &[&[45, 57, 60, 64], &[41, 57, 60, 65], &[48, 55, 60, 64], &[43, 55, 59, 62]];

    /// One beat of `engine`'s sample ducking a sustained noise track on every beat at 120 bpm with
    /// Dream Eater at 100%, in 5 ms steps averaged over beats 4 to 11, once the leveller has
    /// settled: the duck's gain, how far the voices are open, and the chord's RMS on its own.
    struct Bloom {
        duck: Vec<f64>,
        open: Vec<f64>,
        chord: Vec<f64>,
        /// Diagnostics: the raw wet's RMS, the leveller, and the limiter.
        raw: Vec<f64>,
        leveller: Vec<f64>,
        limiter: Vec<f64>,
    }

    const BLOOM_STEP: usize = 240;

    fn bloom(engine: impl Fn() -> Engine) -> Bloom {
        const BEAT: usize = 24_000;
        let beats = 12;
        let input: Vec<f32> = noise(beats * BEAT).iter().map(|v| v * 0.3).collect();
        let (mut ducker, mut eater) = (engine(), engine());
        let steps = BEAT / BLOOM_STEP;
        let mut shape = Bloom {
            duck: vec![0.0; steps],
            open: vec![0.0; steps],
            chord: vec![0.0; steps],
            raw: vec![0.0; steps],
            leveller: vec![0.0; steps],
            limiter: vec![0.0; steps],
        };
        for beat in 0..beats {
            let range = beat * BEAT..(beat + 1) * BEAT;
            let (mut out, mut gain, mut levels, mut probes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for start in range.clone().step_by(BLOOM_STEP) {
                let transport = TransportInfo::playing_at(start as f64 * 2.0 / RATE, 120.0, RATE);
                gain.extend(run(&mut ducker, BLOOM_STEP, transport, ducking(0.0)).0);
                let mut left = input[start..start + BLOOM_STEP].to_vec();
                let mut right = left.clone();
                eater.settings.dream_key = KeyChoice::Fixed(0);
                let report = eater.process(&mut [&mut left[..], &mut right[..]], &transport, || ducking(1.0));
                levels.push(report.dream_level as f64);
                probes.push(std::mem::take(&mut eater.dream.probe));
                out.extend(left);
            }
            if beat < 4 {
                continue;
            }
            let ducked: Vec<f32> = input[range].iter().zip(&gain).map(|(v, g)| v * g).collect();
            for step in 0..steps {
                let window = step * BLOOM_STEP..(step + 1) * BLOOM_STEP;
                // The chord alone: the output less the ducked track, at the share of it that is
                // there in this step, which the makeup moves.
                let (out, ducked) = (&out[window.clone()], &ducked[window.clone()]);
                let share = out.iter().zip(ducked).map(|(a, b)| a * b).sum::<f32>() / ducked.iter().map(|v| v * v).sum::<f32>();
                let chord: Vec<f32> = out.iter().zip(ducked).map(|(a, b)| a - share * b).collect();
                shape.duck[step] += gain[window].iter().map(|&g| g as f64).sum::<f64>() / BLOOM_STEP as f64 / 8.0;
                shape.open[step] += levels[step] / 8.0;
                let p = &probes[step];
                shape.raw[step] += (p.raw_wet_power / p.frames).sqrt() / 8.0;
                shape.leveller[step] += p.leveller / p.frames / 8.0;
                shape.limiter[step] += p.limiter / p.frames / 8.0;
                shape.chord[step] += rms(&chord) as f64 / 8.0;
            }
        }
        shape
    }

    impl Bloom {
        fn ms(step: usize) -> f64 {
            (step * BLOOM_STEP) as f64 * 1_000.0 / RATE
        }

        /// When the duck is deepest, and when it has come 90% of the way back.
        fn duck_ms(&self) -> (f64, f64) {
            let bottom = (0..self.duck.len()).min_by(|&a, &b| self.duck[a].total_cmp(&self.duck[b])).unwrap();
            let depth = 1.0 - self.duck[bottom];
            let back = (bottom..self.duck.len()).find(|&s| 1.0 - self.duck[s] <= 0.1 * depth).unwrap_or(self.duck.len());
            (Self::ms(bottom), Self::ms(back))
        }

        /// When the chord peaks, and how long it takes to rise from 10% to 90% of the way from
        /// its level between hits to that peak.
        fn chord_ms(&self) -> (f64, f64) {
            let peak = (0..self.chord.len()).max_by(|&a, &b| self.chord[a].total_cmp(&self.chord[b])).unwrap();
            let low = self.chord[..=peak].iter().cloned().fold(f64::MAX, f64::min);
            let at = |share: f64| {
                let level = low + share * (self.chord[peak] - low);
                (0..=peak).rev().find(|&s| self.chord[s] <= level).map_or(0, |s| s + 1)
            };
            (Self::ms(peak), Self::ms(at(0.9)) - Self::ms(at(0.1)))
        }
    }

    #[test]
    #[ignore]
    fn dream_eater_bloom_report() {
        let water = crate::preset::Preset::factory("Water").unwrap().sample.unwrap();
        let water = || {
            let mut engine = Engine::new(RATE);
            engine.set_sample(Some(Arc::new(PreparedSample::new(&water, RATE))));
            engine.finish_analysis();
            engine
        };
        for (name, shape) in [("kick", bloom(kick_engine)), ("Water", bloom(water))] {
            println!("{name}:");
            for step in (0..60).step_by(2) {
                println!(
                    "{:4.0} ms: duck {:.2}  open {:.2}  chord {:.4}  raw {:.5}  leveller {:.1}  limiter {:.2}",
                    Bloom::ms(step),
                    shape.duck[step],
                    shape.open[step],
                    shape.chord[step],
                    shape.raw[step],
                    shape.leveller[step],
                    shape.limiter[step]
                );
            }
            println!("duck deepest, 90% back at {:?} ms; chord peak at, 10-90% rise {:?} ms", shape.duck_ms(), shape.chord_ms());
        }
    }

    /// Inside Dream Eater on real recordings, with Water ducking: the factory samples, and any
    /// files in `HAUNTER_REAL_TRACKS`, separated by semicolons.
    #[test]
    #[ignore]
    fn real_material_probe() {
        let water = crate::preset::Preset::factory("Water").unwrap().sample.unwrap();
        let mut paths: Vec<std::path::PathBuf> = ["Water", "Paper", "Stone", "Umber", "Wind"]
            .iter()
            .map(|name| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("presets").join(format!("{name}.wav")))
            .collect();
        if let Ok(list) = std::env::var("HAUNTER_REAL_TRACKS") {
            paths.extend(list.split(';').filter(|p| !p.trim().is_empty()).map(|p| p.trim().into()));
        }
        for path in paths {
            let audio = crate::audio_file::decode_file(&path).unwrap();
            let left = crate::sample::resample(&audio.channels[0], audio.sample_rate, RATE);
            let right = crate::sample::resample(&audio.channels[audio.channels.len() - 1], audio.sample_rate, RATE);
            let frames = 12 * RATE as usize;
            let peak = left.iter().chain(&right).fold(0.0f32, |m, v| m.max(v.abs())).max(1e-9);
            let mut sides: Vec<Vec<f32>> =
                [&left, &right].iter().map(|s| (0..frames).map(|n| 0.5 * s[n % s.len()] / peak).collect()).collect();
            let mut engine = Engine::new(RATE);
            engine.set_sample(Some(Arc::new(PreparedSample::new(&water, RATE))));
            engine.finish_analysis();
            let mut keys = Vec::new();
            let mut beat = 0.0;
            let (l, r) = sides.split_at_mut(1);
            for (left, right) in l[0].chunks_mut(512).zip(r[0].chunks_mut(512)) {
                if beat >= 12.0 && engine.dream.probe.frames == 0.0 {
                    engine.dream.probe = Default::default();
                }
                let transport = TransportInfo::playing_at(beat, 120.0, RATE);
                engine.process(&mut [left, right], &transport, || ducking(1.0));
                beat += 512.0 * 2.0 / RATE;
                if keys.last() != Some(&(engine.dream_key(), engine.detected_key())) {
                    keys.push((engine.dream_key(), engine.detected_key()));
                }
                if beat < 12.0 {
                    engine.dream.probe = Default::default();
                }
            }
            let p = &engine.dream.probe;
            let n = p.frames.max(1.0);
            let db = |power: f64| 10.0 * (power / n).max(1e-30).log10();
            println!(
                "{}: keys {:?}\n  mid {:.1} dB, delayed {:.1} dB, into resonators {:.1} dB, open {:.2}, raw wet {:.1} dB, \
                 ducked track {:.1} dB, leveller {:.2} (capped {:.0}%), makeup {:.2}, limiter {:.2}, wet out {:.1} dB",
                path.file_stem().unwrap().to_string_lossy(),
                keys,
                db(p.mid_power),
                db(p.delayed_power),
                db(p.input_power),
                p.open / n,
                db(p.raw_wet_power),
                db(p.ducked_power),
                p.leveller / n,
                100.0 * p.capped / n,
                p.makeup / n,
                p.limiter / n,
                db(p.wet_out_power),
            );
        }
    }

    #[test]
    #[ignore]
    fn dream_eater_report() {
        let playing = TransportInfo::playing_at(0.0, 120.0, RATE);
        let mut in_key = chord_track(A_MINOR, 8 * RATE as usize);
        noise(in_key.len()).iter().zip(&mut in_key).for_each(|(hiss, v)| *v += 0.03 * hiss);
        let materials = [
            ("test track, 2 s", track(96_000)),
            ("test track, last 6 s", track(576_000)),
            ("noise", noise(96_000).iter().map(|v| v * 0.3).collect()),
            ("A minor chords and hiss, last 4 s", in_key),
        ];
        // Both channels, measured from `skip` on.
        let both = |engine: &mut Engine, input: &[f32], amount: f32, skip: usize| {
            let output = process_both(engine, input, playing, || ducking(amount));
            let (left, right) = output.split_at(input.len());
            [&left[skip..], &right[skip..]].concat()
        };
        for (name, input) in materials {
            println!("{name}:");
            let skip = if input.len() > 96_000 { input.len() / 2 } else { 0 };
            let dry = both(&mut kick_engine(), &input, 0.0, skip);
            for amount in [0.1f32, 0.3, 0.5, 0.7, 0.9, 1.0] {
                let mut engine = kick_engine();
                let eaten = both(&mut engine, &input, amount, skip);
                let (eaten, dry) = (&eaten[..], &dry[..]);
                let difference: Vec<f32> = eaten.iter().zip(dry).map(|(a, b)| a - b).collect();
                let dip = crate::dream::dry_gain(amount as f64) as f32;
                let wet: Vec<f32> = eaten.iter().zip(dry).map(|(a, b)| a - dip * b).collect();
                let correlation = wet.iter().zip(dry).map(|(w, d)| w * d).sum::<f32>()
                    / (wet.len() as f32 * rms(&wet) * rms(dry)).max(1e-20);
                print!("  wet {:.3} corr {:+.2}", rms(&wet) / rms(dry), correlation);
                println!(
                    "  {amount:.1}: diff {:.3}  loudness {:+.2} dB  peak {:.3} (dry {:.3})  >5k {:+.2} dB  key {}  leveller {:.2}",
                    rms(&difference) / rms(dry),
                    20.0 * (rms(eaten) / rms(dry)).log10(),
                    eaten.iter().fold(0.0f32, |m, v| m.max(v.abs())),
                    dry.iter().fold(0.0f32, |m, v| m.max(v.abs())),
                    20.0 * (band_rms(eaten, 5_000.0) / band_rms(dry, 5_000.0)).log10(),
                    engine.dream_key(),
                    engine.dream.leveller(),
                );
                println!("    makeup {:.2}", engine.dream.makeup());
            }
        }
        println!("worst-case peak at 1.0: {:.3}", worst_case().1);
        let mut engine = kick_engine();
        let input = track(576_000);
        let keys: Vec<_> = input
            .chunks(48_000)
            .enumerate()
            .map(|(second, chunk)| {
                let transport = TransportInfo::playing_at(second as f64 * 2.0, 120.0, RATE);
                process_both(&mut engine, chunk, transport, || ducking(1.0));
                (engine.dream_key(), engine.detected_key())
            })
            .collect();
        println!("test track keys by second: {keys:?}");
    }
}
