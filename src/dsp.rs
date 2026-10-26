//! Ghostpump's audio engine. Nothing here allocates while processing.

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

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayMode {
    #[id = "one-shot"]
    #[name = "One-shot"]
    OneShot,
    #[id = "loop"]
    #[name = "Loop"]
    Loop,
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

/// High-pass then low-pass on the sample before it reaches the follower.
#[derive(Debug, Clone, Default)]
pub struct KeyFilter {
    high_pass: Svf,
    low_pass: Svf,
    high_pass_on: bool,
    low_pass_on: bool,
}

impl KeyFilter {
    pub fn set(&mut self, high_pass_hz: f32, low_pass_hz: f32, sample_rate: f64) {
        self.high_pass_on = high_pass_hz > HP_OFF_HZ;
        self.low_pass_on = low_pass_hz < LP_OFF_HZ;
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

/// Everything that changes at most once per block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub rate: SyncRate,
    pub mode: PlayMode,
    /// Region start as a fraction of the sample length.
    pub start: f32,
    /// Region end as a fraction of the sample length.
    pub end: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub high_pass_hz: f32,
    pub low_pass_hz: f32,
    pub invert: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            rate: SyncRate::Quarter,
            mode: PlayMode::OneShot,
            start: 0.0,
            end: 1.0,
            attack_ms: 1.0,
            release_ms: 80.0,
            high_pass_hz: HP_OFF_HZ,
            low_pass_hz: LP_OFF_HZ,
            invert: false,
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
    pub output: f32,
}

impl Default for FrameGains {
    fn default() -> Self {
        FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, output: 1.0 }
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
    /// The lowest gain applied to the track in this block, before output gain.
    pub min_track_gain: f32,
    /// Playback position as a fraction of the sample, if a voice is sounding.
    pub playhead: Option<f32>,
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    pos: usize,
    /// Where a fade-in started, after a trigger into the middle of the sample or a loop wrap.
    fade_in_from: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct Tail {
    voice: Voice,
    remaining: usize,
}

/// Short linear fades that stop clicks where playback starts or stops mid-waveform.
fn declick_gain(pos: usize, fade_in_from: Option<usize>, end: usize, fades_out: bool, declick: usize) -> f32 {
    let mut gain = 1.0;
    if let Some(from) = fade_in_from {
        let done = pos.saturating_sub(from) + 1;
        if done < declick {
            gain *= done as f32 / declick as f32;
        }
    }
    let left = end.saturating_sub(pos);
    if fades_out && left < declick {
        gain *= left as f32 / declick as f32;
    }
    gain
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct AnalysisKey {
    sample_id: u64,
    start: usize,
    end: usize,
    fades_out: bool,
    declick: usize,
    high_pass_hz: f32,
    low_pass_hz: f32,
    attack_ms: f32,
    release_ms: f32,
}

#[derive(Debug, Clone)]
struct RunningAnalysis {
    key: AnalysisKey,
    pos: usize,
    filter: KeyFilter,
    follower: EnvelopeFollower,
    peak: f64,
}

/// Finds the follower's peak over the region, so Depth means "this much at the sample's peak".
/// Runs a few thousand frames per block; settings that change mid-run start a fresh run after.
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
                let mut filter = KeyFilter::default();
                filter.set(wanted.high_pass_hz, wanted.low_pass_hz, sample_rate);
                let mut follower = EnvelopeFollower::default();
                follower.set(wanted.attack_ms, wanted.release_ms, sample_rate);
                self.running = Some(RunningAnalysis { key: wanted, pos: wanted.start, filter, follower, peak: 0.0 });
            }
            let run = self.running.as_mut().unwrap();
            if run.key.sample_id != sample.id {
                self.running = None;
                continue;
            }
            let stop = run.key.end.min(sample.frames()).min(run.pos.saturating_add(budget));
            let key = run.key;
            let fade_in_from = (key.start > 0).then_some(key.start);
            for frame in run.pos..stop {
                let gain = declick_gain(frame, fade_in_from, key.end, key.fades_out, key.declick);
                let level = run.follower.process(run.filter.process((sample.key[frame] * gain) as f64));
                run.peak = run.peak.max(level);
            }
            budget -= stop - run.pos;
            run.pos = stop;
            if run.pos < run.key.end.min(sample.frames()) {
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
    filter: KeyFilter,
    follower: EnvelopeFollower,
    analysis: PeakAnalysis,
    /// Highest follower level since the sample changed, used until the analysis finishes.
    live_peak: f64,
    declick: usize,
    region: (usize, usize),
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
            filter: KeyFilter::default(),
            follower: EnvelopeFollower::default(),
            analysis: PeakAnalysis::default(),
            live_peak: 0.0,
            declick: 1,
            region: (0, 0),
        };
        engine.prepare(sample_rate);
        engine
    }

    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.declick = ((DECLICK_SECONDS * sample_rate).round() as usize).max(1);
        self.reset();
    }

    pub fn reset(&mut self) {
        self.clock.reset();
        self.voice = None;
        self.tail = None;
        self.filter.reset();
        self.follower.reset();
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

    fn fades_out(&self, frames: usize) -> bool {
        self.settings.mode == PlayMode::Loop || self.region.1 < frames
    }

    fn analysis_key(&self, sample: &PreparedSample) -> AnalysisKey {
        AnalysisKey {
            sample_id: sample.id,
            start: self.region.0,
            end: self.region.1,
            fades_out: self.fades_out(sample.frames()),
            declick: self.declick,
            high_pass_hz: self.settings.high_pass_hz,
            low_pass_hz: self.settings.low_pass_hz,
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
        self.filter.set(settings.high_pass_hz, settings.low_pass_hz, self.sample_rate);
        self.follower.set(settings.attack_ms, settings.release_ms, self.sample_rate);
        if let Some(sample) = self.sample.clone() {
            self.update_region(sample.frames());
            let key = self.analysis_key(&sample);
            self.analysis.step(&sample, key, self.sample_rate, ANALYSIS_BUDGET);
        }
    }

    /// Ducks `channels` in place and mixes in the sample. `gains` is called once per frame.
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
        let grid = match (transport.playing, transport.beat, transport.beats_per_frame) {
            (true, Some(beat), Some(step)) if step > 0.0 => Some((beat, step)),
            _ => None,
        };
        if grid.is_none() {
            self.clock.reset();
        }

        let mut min_track_gain = 1.0f32;
        for frame in 0..frames {
            let trigger = match grid {
                Some((beat, step)) => self.clock.tick(beat + frame as f64 * step, step, interval),
                None => false,
            };
            let gains = gains();
            let (track_gain, add_left, add_right) = self.frame(trigger, transport.playing, &gains);
            min_track_gain = min_track_gain.min(track_gain);
            for (index, channel) in channels.iter_mut().enumerate() {
                let add = match (channel_count, index) {
                    (1, _) => 0.5 * (add_left + add_right),
                    (_, 0) => add_left,
                    (_, 1) => add_right,
                    _ => 0.0,
                };
                channel[frame] = (channel[frame] * track_gain + add) * gains.output;
            }
        }

        let playhead = match (&self.sample, &self.voice) {
            (Some(sample), Some(voice)) if sample.frames() > 0 => Some(voice.pos as f32 / sample.frames() as f32),
            _ => None,
        };
        BlockReport { min_track_gain, playhead }
    }

    fn fade_gain(&self, voice: &Voice, frames: usize) -> f32 {
        declick_gain(voice.pos, voice.fade_in_from, self.region.1, self.fades_out(frames), self.declick)
    }

    /// Returns (track gain, sample left, sample right) for one frame.
    fn frame(&mut self, trigger: bool, playing: bool, gains: &FrameGains) -> (f32, f32, f32) {
        let Some(sample) = self.sample.clone() else {
            return (1.0, 0.0, 0.0);
        };
        let frames = sample.frames();
        let (start, end) = self.region;

        if trigger {
            if let Some(voice) = self.voice.take() {
                self.tail = Some(Tail { voice, remaining: self.declick });
            }
            self.voice = Some(Voice { pos: start, fade_in_from: (start > 0).then_some(start) });
        }

        let (mut left, mut right, mut key) = (0.0f32, 0.0f32, 0.0f32);
        if let Some(mut voice) = self.voice {
            if voice.pos < start {
                voice = Voice { pos: start, fade_in_from: Some(start) };
            }
            if voice.pos < end {
                let gain = self.fade_gain(&voice, frames);
                left += sample.left[voice.pos] * gain;
                right += sample.right[voice.pos] * gain;
                key += sample.key[voice.pos] * gain;
                voice.pos += 1;
            }
            self.voice = if voice.pos < end {
                Some(voice)
            } else if self.settings.mode == PlayMode::Loop && playing {
                Some(Voice { pos: start, fade_in_from: Some(start) })
            } else {
                None
            };
        }
        if let Some(mut tail) = self.tail {
            if tail.voice.pos < end && tail.remaining > 0 {
                let gain = self.fade_gain(&tail.voice, frames) * tail.remaining as f32 / self.declick as f32;
                left += sample.left[tail.voice.pos] * gain;
                right += sample.right[tail.voice.pos] * gain;
                key += sample.key[tail.voice.pos] * gain;
                tail.voice.pos += 1;
                tail.remaining -= 1;
                self.tail = Some(tail);
            } else {
                self.tail = None;
            }
        }

        let level = self.follower.process(self.filter.process(key as f64));
        self.live_peak = self.live_peak.max(level);
        let peak = self.normaliser();
        let normalised = if peak > 1e-9 { (level / peak).min(1.0) as f32 } else { 0.0 };
        let shape = if self.settings.invert { 1.0 - normalised } else { normalised };
        let ducked = 1.0 - gains.depth * shape;
        let track_gain = 1.0 - gains.mix * (1.0 - ducked);
        (track_gain, left * gains.sample_level, right * gains.sample_level)
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
        FrameGains { depth: 0.0, mix: 0.0, sample_level: 1.0, output: 1.0 }
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
        let gains = FrameGains { depth: 0.75, mix: 1.0, sample_level: 0.0, output: 1.0 };
        let (left, right) = run(&mut engine, 24_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let lowest = left.iter().cloned().fold(f32::MAX, f32::min);
        assert!((lowest - 0.25).abs() < 1e-4, "75% depth leaves 25% at the peak, got {lowest}");
        assert_eq!(left, right);
        assert!(left[0] > 0.9, "the duck follows the attack, it doesn't jump");
        assert!(left[23_999] > 0.99, "the track recovers after the kick");
    }

    #[test]
    fn invert_opens_the_track_at_the_peak() {
        let mut engine = Engine::new(RATE);
        engine.settings.invert = true;
        engine.set_sample(Some(prepared(vec![kick(400.0)])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, output: 1.0 };
        let (left, _) = run(&mut engine, 24_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let highest = left.iter().cloned().fold(f32::MIN, f32::max);
        assert!((highest - 1.0).abs() < 1e-4);
        assert!(left[0] < 0.1 && left[23_999] < 0.01, "quiet sample means a ducked track");
    }

    #[test]
    fn mix_and_output_scale_the_effect() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(400.0)])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 1.0, mix: 0.5, sample_level: 0.0, output: 0.5 };
        let (left, _) = run(&mut engine, 24_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let lowest = left.iter().cloned().fold(f32::MAX, f32::min);
        assert!((lowest - 0.25).abs() < 1e-4, "half the duck at half volume, got {lowest}");
    }

    #[test]
    fn high_pass_removes_a_low_kick_from_the_key() {
        let mut engine = Engine::new(RATE);
        let mut sample = kick(300.0);
        // A short 4 kHz click at the end of the kick.
        for (n, value) in sample.iter_mut().enumerate().skip(9_600).take(480) {
            *value += 0.05 * (2.0 * std::f64::consts::PI * 4000.0 * n as f64 / RATE).sin() as f32;
        }
        engine.settings.high_pass_hz = 1500.0;
        engine.settings.release_ms = 5.0;
        engine.set_sample(Some(prepared(vec![sample])));
        engine.finish_analysis();
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, output: 1.0 };
        let (left, _) = run(&mut engine, 14_400, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let early = left[..4_800].iter().cloned().fold(f32::MAX, f32::min);
        let click = left[9_600..10_080].iter().cloned().fold(f32::MAX, f32::min);
        assert!(early > 0.8, "the kick's low end barely ducks once high-passed: {early}");
        assert!(click < 0.05, "the click becomes the key: {click}");
    }

    #[test]
    fn one_shot_plays_the_region_once_per_trigger() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![ramp(1_000)])));
        let left = play(&mut engine, 24_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        for n in 0..1_000 {
            assert_eq!(left[n], (n + 1) as f32 / 1_000.0);
        }
        assert!(left[1_000..].iter().all(|&v| v == 0.0), "silent after one pass");
    }

    #[test]
    fn loop_repeats_the_region_until_the_next_trigger() {
        let mut engine = Engine::new(RATE);
        engine.settings.mode = PlayMode::Loop;
        engine.set_sample(Some(prepared(vec![vec![0.5; 1_000]])));
        let left = play(&mut engine, 30_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        let declick = (DECLICK_SECONDS * RATE) as usize;
        // Away from the 1 ms fades at each wrap, the loop plays continuously.
        for pass in 0..20 {
            let middle = pass * 1_000 + 500;
            assert_eq!(left[middle], 0.5, "pass {pass}");
        }
        assert!(left[999] < 0.5 && left[1_000] < 0.5, "wraps are declicked");
        assert_eq!(left[1_000 + declick + 1], 0.5);
        // The trigger at frame 24000 restarts the region from its start.
        assert_eq!(left[24_000 + 500], 0.5);
    }

    #[test]
    fn loop_restarts_from_the_region_start_on_each_trigger() {
        let mut engine = Engine::new(RATE);
        engine.settings.mode = PlayMode::Loop;
        engine.set_sample(Some(prepared(vec![ramp(10_000)])));
        // 1/16 at 120 bpm is 6000 frames, shorter than the 10000-frame loop.
        engine.settings.rate = SyncRate::Sixteenth;
        let left = play(&mut engine, 12_000, TransportInfo::playing_at(0.0, 120.0, RATE));
        let declick = (DECLICK_SECONDS * RATE) as usize;
        assert_eq!(left[5_999], 6_000.0 / 10_000.0);
        // After the retrigger's crossfade, the voice is back at the start.
        assert_eq!(left[6_000 + declick + 10], (declick + 11) as f32 / 10_000.0);
    }

    #[test]
    fn loop_finishes_its_pass_when_the_transport_stops() {
        let mut engine = Engine::new(RATE);
        engine.settings.mode = PlayMode::Loop;
        engine.set_sample(Some(prepared(vec![vec![0.5; 1_000]])));
        play(&mut engine, 500, TransportInfo::playing_at(0.0, 120.0, RATE));
        let left = play(&mut engine, 2_000, TransportInfo::stopped());
        assert_eq!(left[100], 0.5);
        assert!(left[500..].iter().all(|&v| v == 0.0));
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
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, output: 1.0 };
        let (left, _) = run(&mut engine, 12_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        let lowest = left.iter().cloned().fold(f32::MAX, f32::min);
        assert!(lowest < 1e-4, "{lowest}");
    }

    #[test]
    fn stereo_samples_blend_into_matching_channels_and_mono_tracks_get_the_sum() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![vec![0.4; 100], vec![-0.2; 100]])));
        let gains = FrameGains { depth: 0.0, mix: 1.0, sample_level: 0.5, output: 1.0 };
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
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 1.0, output: 0.5 };
        let (left, _) = run(&mut engine, 1_000, TransportInfo::playing_at(0.0, 120.0, RATE), gains);
        assert!(left.iter().all(|&v| v == 0.5));
    }

    #[test]
    fn analysis_runs_incrementally_on_the_audio_thread() {
        let mut engine = Engine::new(RATE);
        engine.set_sample(Some(prepared(vec![kick(2_000.0)])));
        let gains = FrameGains { depth: 1.0, mix: 1.0, sample_level: 0.0, output: 1.0 };
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
}
