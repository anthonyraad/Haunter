//! Dream Eater: glassy resonances on the track, tuned to its key and played by the sample.
//!
//! The track's key is estimated from its audio. Three resonator voices, the root, fifth, and ninth
//! of that key's major pentatonic, listen to a delayed copy of the track, taken before the duck,
//! and each hit of the sample opens them, so they swell and ring out in tune. The delay lands the
//! track's energy in the gap each duck opens. Each voice is a detuned pair, one per side, for a
//! stereo shimmer. The chord is added after the duck, so it blooms in the space each hit opens.
//! Nothing is generated here but the ringing of the track itself: there is no noise source, so
//! silence in is silence out. Nothing here allocates while processing.

use serde::{Deserialize, Serialize};
use std::f64::consts::{PI, TAU};

use crate::dsp::{one_pole, Svf};

/// Pitch class names, C first.
pub const KEY_NAMES: [&str; 12] = ["C", "C#/Db", "D", "D#/Eb", "E", "F", "F#/Gb", "G", "G#/Ab", "A", "A#/Bb", "B"];
/// How keys are stored in the plugin state.
const KEY_IDS: [&str; 12] = ["c", "c#", "d", "d#", "e", "f", "f#", "g", "g#", "a", "a#", "b"];
/// Where Dream Eater tunes while no key has been heard with confidence.
pub const FALLBACK_KEY: u8 = 0;

/// The key Dream Eater tunes to. A key is a major key's tonic, and stands for its relative minor
/// too, since both share one pentatonic scale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum KeyChoice {
    /// Follow the track's detected key.
    #[default]
    Auto,
    /// A pitch class, C = 0.
    Fixed(u8),
}

impl KeyChoice {
    /// Auto, then the twelve keys from C.
    pub fn all() -> impl Iterator<Item = KeyChoice> {
        std::iter::once(KeyChoice::Auto).chain((0..12).map(KeyChoice::Fixed))
    }

    pub fn label(self) -> String {
        match self {
            KeyChoice::Auto => "Auto".to_owned(),
            KeyChoice::Fixed(key) => {
                let key = key as usize % 12;
                format!("{} major / {} minor", KEY_NAMES[key], KEY_NAMES[(key + 9) % 12])
            }
        }
    }

    pub fn to_bits(self) -> u8 {
        match self {
            KeyChoice::Auto => u8::MAX,
            KeyChoice::Fixed(key) => key % 12,
        }
    }

    pub fn from_bits(bits: u8) -> Self {
        if bits < 12 {
            KeyChoice::Fixed(bits)
        } else {
            KeyChoice::Auto
        }
    }
}

impl From<KeyChoice> for String {
    fn from(choice: KeyChoice) -> String {
        match choice {
            KeyChoice::Auto => "auto".to_owned(),
            KeyChoice::Fixed(key) => KEY_IDS[key as usize % 12].to_owned(),
        }
    }
}

impl TryFrom<String> for KeyChoice {
    type Error = String;

    fn try_from(id: String) -> Result<Self, String> {
        let id = id.trim().to_ascii_lowercase();
        if id == "auto" {
            return Ok(KeyChoice::Auto);
        }
        KEY_IDS
            .iter()
            .position(|known| *known == id)
            .map(|key| KeyChoice::Fixed(key as u8))
            .ok_or_else(|| format!("{id} is not a key"))
    }
}

/// The analysis runs on the track decimated to about this rate.
const ANALYSIS_HZ: f64 = 6_000.0;
const FFT_SIZE: usize = 4_096;
const FFT_MASK: usize = FFT_SIZE - 1;
/// Decimated frames between analyses.
const HOP: usize = 1_024;
const LOWEST_HZ: f64 = 90.0;
const HIGHEST_HZ: f64 = 2_000.0;
/// How long the pitch-class profile remembers what it heard.
const MEMORY_SECONDS: f64 = 4.0;
/// Tonal audio heard before the first estimate.
const WARM_UP_SECONDS: f64 = 1.5;
/// How long a new key must stay the confident best before Dream Eater moves to it.
const HOLD_SECONDS: f64 = 1.5;
/// The best key's correlation with its profile, and its lead over the best key with another
/// pentatonic scale, needed to count as confident.
const MIN_CORRELATION: f64 = 0.6;
const MIN_MARGIN: f64 = 0.04;
/// Spectral peaks quieter than this share of the loudest one are ignored.
const PEAK_FLOOR: f32 = 0.05;
/// Peaks further than this from a semitone, in semitones, are ignored.
const PITCH_TOLERANCE: f64 = 0.35;
/// Below this RMS an analysis frame counts as silence and changes nothing.
const SILENCE: f32 = 1e-4;
/// Spectral flatness from which a frame counts as noise and changes nothing, and the span below
/// it over which a frame counts more and more until it is fully tonal. White noise sits near 0.5.
const NOISY_FLATNESS: f64 = 0.4;
const TONAL_SPAN: f64 = 0.25;
/// Krumhansl and Kessler's key profiles, from the tonic up.
const MAJOR_PROFILE: [f64; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const MINOR_PROFILE: [f64; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

/// Pitch-class offsets of the first six harmonics, and how much each counts in a key template.
const HARMONIC_OFFSETS: [usize; 6] = [0, 0, 7, 0, 4, 7];
const HARMONIC_DECAY: f64 = 0.6;

/// A key profile as its notes' spectra would show it: each note also lends weight to the pitch
/// classes of its overtones, so a strong fifth from the tonic's third harmonic isn't heard as a
/// key a fifth up.
fn template(profile: [f64; 12]) -> [f64; 12] {
    let mut out = [0.0; 12];
    for (note, weight) in profile.iter().enumerate() {
        let mut share = 1.0;
        for offset in HARMONIC_OFFSETS {
            out[(note + offset) % 12] += weight * share;
            share *= HARMONIC_DECAY;
        }
    }
    centred(out)
}

/// Centred and scaled to unit length, so a dot product with a centred profile is a correlation.
fn centred(values: [f64; 12]) -> [f64; 12] {
    let mean = values.iter().sum::<f64>() / 12.0;
    let mut out = values.map(|v| v - mean);
    let length = out.iter().map(|v| v * v).sum::<f64>().sqrt();
    if length > 1e-12 {
        out.iter_mut().for_each(|v| *v /= length);
    }
    out
}

/// In-place radix-2 FFT. `cos` and `sin` hold the first half of one turn in `re.len()` steps.
fn fft(re: &mut [f32], im: &mut [f32], cos: &[f32], sin: &[f32]) {
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
        let half = length / 2;
        let stride = n / length;
        for start in (0..n).step_by(length) {
            for k in 0..half {
                let (wr, wi) = (cos[k * stride], -sin[k * stride]);
                let (a, b) = (start + k, start + k + half);
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

/// Estimates a track's key from a pitch-class profile of its spectral peaks over the last few
/// seconds, correlated with the Krumhansl-Kessler profiles. Keys are reported as the tonic of the
/// major key whose pentatonic scale fits, so A minor reads as C.
#[derive(Debug, Clone, Default)]
pub struct KeyDetector {
    decimation: usize,
    countdown: usize,
    anti_alias: [Svf; 4],
    /// The last `FFT_SIZE` decimated frames, a ring.
    history: Vec<f32>,
    write: usize,
    stored: usize,
    since_analysis: usize,
    analysis_rate: f64,
    window: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
    lowest_bin: usize,
    highest_bin: usize,
    profiles: [[f64; 12]; 2],
    chroma: [f64; 12],
    /// How much of the profile survives one fully tonal analysis.
    memory: f64,
    /// Fully tonal analyses so far.
    heard: f64,
    warm_up: f64,
    hold: usize,
    /// A confident key that isn't the current one, and for how many analyses in a row.
    candidate: Option<(u8, usize)>,
    key: Option<u8>,
}

impl KeyDetector {
    /// Allocates for `sample_rate`. Not for the audio thread. Forgets any key it had.
    pub fn prepare(&mut self, sample_rate: f64) {
        self.decimation = ((sample_rate / ANALYSIS_HZ).round() as usize).max(1);
        self.analysis_rate = sample_rate / self.decimation as f64;
        for filter in &mut self.anti_alias {
            filter.set(0.4 * self.analysis_rate, sample_rate);
        }
        self.history = vec![0.0; FFT_SIZE];
        self.window = (0..FFT_SIZE).map(|n| (0.5 - 0.5 * (TAU * n as f64 / FFT_SIZE as f64).cos()) as f32).collect();
        self.re = vec![0.0; FFT_SIZE];
        self.im = vec![0.0; FFT_SIZE];
        self.cos = (0..FFT_SIZE / 2).map(|k| (TAU * k as f64 / FFT_SIZE as f64).cos() as f32).collect();
        self.sin = (0..FFT_SIZE / 2).map(|k| (TAU * k as f64 / FFT_SIZE as f64).sin() as f32).collect();
        let bin = |hz: f64| hz * FFT_SIZE as f64 / self.analysis_rate;
        self.lowest_bin = (bin(LOWEST_HZ).ceil() as usize).max(2);
        self.highest_bin = (bin(HIGHEST_HZ).floor() as usize).min(FFT_SIZE / 2 - 2);
        self.profiles = [template(MAJOR_PROFILE), template(MINOR_PROFILE)];
        let hop_seconds = HOP as f64 / self.analysis_rate;
        self.memory = (-hop_seconds / MEMORY_SECONDS).exp();
        self.warm_up = WARM_UP_SECONDS / hop_seconds;
        self.hold = (HOLD_SECONDS / hop_seconds).ceil() as usize;
        self.chroma = [0.0; 12];
        self.heard = 0.0;
        self.candidate = None;
        self.key = None;
        self.reset();
    }

    /// Clears the audio it holds but keeps the key, so a transport restart doesn't lose it.
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.write = 0;
        self.stored = 0;
        self.since_analysis = 0;
        self.countdown = self.decimation;
        for filter in &mut self.anti_alias {
            filter.reset();
        }
    }

    /// The confident key, if there has been one.
    pub fn key(&self) -> Option<u8> {
        self.key
    }

    /// Takes one frame of the track, in mono.
    pub fn push(&mut self, input: f32) {
        let mut value = input as f64;
        for filter in &mut self.anti_alias {
            value = filter.process(value).0;
        }
        self.countdown -= 1;
        if self.countdown > 0 {
            return;
        }
        self.countdown = self.decimation;
        self.history[self.write] = value as f32;
        self.write = (self.write + 1) & FFT_MASK;
        self.stored = (self.stored + 1).min(FFT_SIZE);
        self.since_analysis += 1;
        if self.since_analysis >= HOP && self.stored == FFT_SIZE {
            self.since_analysis = 0;
            self.analyse();
        }
    }

    /// Adds one windowed spectrum to the profile, weighted by how tonal it is, and updates the key.
    fn analyse(&mut self) {
        let mut power = 0.0f32;
        for n in 0..FFT_SIZE {
            let value = self.history[(self.write + n) & FFT_MASK];
            power += value * value;
            self.re[n] = value * self.window[n];
            self.im[n] = 0.0;
        }
        if (power / FFT_SIZE as f32).sqrt() < SILENCE {
            return;
        }
        fft(&mut self.re, &mut self.im, &self.cos, &self.sin);
        let (lowest, highest) = (self.lowest_bin, self.highest_bin);
        let (mut loudest, mut log_sum, mut sum) = (0.0f32, 0.0f64, 0.0f64);
        for k in lowest - 1..=highest + 1 {
            let bin_power = self.re[k] * self.re[k] + self.im[k] * self.im[k];
            self.re[k] = bin_power.sqrt();
            if (lowest..=highest).contains(&k) {
                loudest = loudest.max(self.re[k]);
                log_sum += (bin_power as f64 + 1e-20).ln();
                sum += bin_power as f64;
            }
        }
        let bins = (highest - lowest + 1) as f64;
        let flatness = (log_sum / bins).exp() / (sum / bins).max(1e-30);
        let tonal = ((NOISY_FLATNESS - flatness) / TONAL_SPAN).clamp(0.0, 1.0);
        if tonal <= 0.0 {
            return;
        }

        let mut frame = [0.0f64; 12];
        let magnitude = &self.re;
        for k in lowest..=highest {
            let (before, peak, after) = (magnitude[k - 1], magnitude[k], magnitude[k + 1]);
            if peak <= before || peak < after || peak < loudest * PEAK_FLOOR {
                continue;
            }
            let curve = before - 2.0 * peak + after;
            let offset = if curve < 0.0 { (0.5 * (before - after) / curve) as f64 } else { 0.0 };
            let hz = (k as f64 + offset) * self.analysis_rate / FFT_SIZE as f64;
            let pitch = 69.0 + 12.0 * (hz / 440.0).log2();
            let nearest = pitch.round();
            if (pitch - nearest).abs() <= PITCH_TOLERANCE {
                frame[(nearest as i64).rem_euclid(12) as usize] += peak as f64;
            }
        }
        let total: f64 = frame.iter().sum();
        if total <= 0.0 {
            return;
        }
        let blend = (1.0 - self.memory) * tonal;
        for (held, new) in self.chroma.iter_mut().zip(frame) {
            *held += blend * (new / total - *held);
        }
        self.heard += tonal;
        if self.heard < self.warm_up || tonal < 0.5 {
            return;
        }
        match self.estimate() {
            Some(key) if Some(key) == self.key => self.candidate = None,
            Some(key) => {
                let count = match self.candidate {
                    Some((candidate, count)) if candidate == key => count + 1,
                    _ => 1,
                };
                if count >= self.hold {
                    self.key = Some(key);
                    self.candidate = None;
                } else {
                    self.candidate = Some((key, count));
                }
            }
            None => self.candidate = None,
        }
    }

    /// How well each key's pentatonic scale fits the profile: the best correlation of its major
    /// key or its relative minor.
    fn fits(&self) -> [f64; 12] {
        let chroma = centred(self.chroma);
        let mut by_key = [f64::MIN; 12];
        for tonic in 0..12 {
            for (minor, profile) in self.profiles.iter().enumerate() {
                let fit: f64 = (0..12).map(|step| chroma[(tonic + step) % 12] * profile[step]).sum();
                let key = if minor == 1 { (tonic + 3) % 12 } else { tonic };
                by_key[key] = by_key[key].max(fit);
            }
        }
        by_key
    }

    /// The best-fitting key, if it fits clearly enough.
    fn estimate(&self) -> Option<u8> {
        let by_key = self.fits();
        let best = (0..12).max_by(|a, b| by_key[*a].total_cmp(&by_key[*b])).unwrap();
        let runner_up = (0..12).filter(|key| *key != best).map(|key| by_key[key]).fold(f64::MIN, f64::max);
        (by_key[best] >= MIN_CORRELATION && by_key[best] - runner_up >= MIN_MARGIN).then_some(best as u8)
    }
}

/// Semitones above the root of each voice: root, fifth, and ninth.
const VOICING: [f64; VOICES] = [0.0, 7.0, 14.0];
const VOICES: usize = 3;
/// Each voice rings at its pitch and, more quietly and briefly, an octave up, for a glassy edge.
/// (frequency ratio, level, share of the tail)
const PARTIALS: [(f64, f64, f64); 2] = [(1.0, 1.0, 1.0), (2.0, 0.3, 0.5)];
/// Pan of each voice's partials, -1 left to 1 right. The root sits in the middle and the fifth
/// and ninth lean apart, with their octaves on the other side.
const PAN: [[f64; 2]; VOICES] = [[0.0, 0.0], [-0.6, 0.4], [0.6, -0.4]];
/// Every partial is a pair of resonators, one per side, detuned against each other by up to this
/// many cents at a slow rate of its voice's own, so the chord shimmers across the stereo field.
const SHIMMER_CENTS: f64 = 5.0;
const SHIMMER_HZ: [f64; VOICES] = [0.13, 0.19, 0.23];
/// How long the voices ring, to -60 dB, at the bottom and top of the knob.
const TAIL_SHORTEST: f64 = 0.3;
const TAIL_LONGEST: f64 = 0.9;
/// How far the track reaches the voices between hits, as a share of a full hit.
const RESTING_OPEN: f64 = 0.1;
const SWELL_ATTACK_MS: f32 = 25.0;
const SWELL_RELEASE_MS: f32 = 140.0;
/// The resonators hear the track, before the duck, this long after it played: the follower's
/// Attack, which is when the duck has come in, plus two of the swell's attack time constants,
/// by when the voices are about three-quarters open. So the track's energy arrives in the gap
/// right after each duck, as the hit opens the voices. 51 ms at the default 1 ms Attack.
const EXCITE_SWELLS: f64 = 2.0;
/// The longest Attack, and so the longest delay the delay line has to hold.
const LONGEST_ATTACK_MS: f64 = 100.0;
/// How quickly the delay follows a change of Attack, so it glides instead of jumping.
const DELAY_GLIDE_MS: f32 = 50.0;
/// One side of each partial hears the track a quarter of the partial's cycle plus whole cycles
/// adding up to about this long after the other, so the two sides ring a quarter-cycle apart:
/// uncorrelated, for a wide chord, yet nothing cancels when they are summed to mono.
const STEREO_OFFSET_MS: f64 = 6.0;
/// Room in the delay line for that offset at the lowest partial.
const STEREO_ROOM_MS: f64 = 12.0;
/// How long a new key takes to take over the track's input from the old one.
const CROSSFADE_SECONDS: f64 = 0.4;
/// The shortest time between retunes, so the bank being retuned has rung out.
const RETUNE_GAP_SECONDS: f64 = 2.0;
const WET_HIGH_PASS_HZ: f64 = 180.0;
/// A first-order shelf lifts the wet above this by `AIR_GAIN`.
const AIR_HZ: f64 = 1_500.0;
const AIR_GAIN: f64 = 1.41;
/// How loud the chord is at 100%, relative to the track after the duck: 4 dB under it, so the
/// output differs from Off by about 3 to 4 dB on real material. The chord is added after the duck,
/// so it blooms in the space each hit opens, and the track is turned down as the chord comes up (by
/// about 2.4 dB at 100%) so the output stays about as loud as Off.
const WET_LEVEL: f64 = 0.63;
/// The most the leveller turns the chord up, 60 dB. The resonators are only a few hertz wide, so
/// they keep a tiny share of a real track's power: 25 to 45 dB below it for textures, drums, and
/// strings off the chord's notes. The leveller has to be free to bring that up to `WET_LEVEL`, or
/// the chord stays inaudible on anything but notes right on its pitches. It still makes nothing of
/// silence, which has no power to bring up.
const MAX_GAIN: f64 = 1_000.0;
/// The leveller turns the chord down at once but up only this fast, from no less than unity, so
/// it never runs ahead of resonators still ringing up when the track starts or comes back after a
/// gap: the chord blooms in over a second instead of bursting out.
const LEVELLER_RISE_DB_PER_SECOND: f64 = 60.0;
/// How long the leveller listens to judge loudness.
/// Long enough to hear several hits, so the leveller leaves each hit's swell alone.
const LOUDNESS_MS: f32 = 1_500.0;
/// The leveller also follows the power over this, so a sudden rise in the wet is caught...
const SURGE_MS: f32 = 50.0;
/// ...before it is more than this many times the power the leveller allows.
const POWER_SURGE: f64 = 4.0;
/// The leveller forgets loud power once it is gone from this many blocks of `SURGE_BLOCK_MS`.
const POWER_BLOCKS: usize = 4;
const SURGE_BLOCK_MS: f64 = 250.0;
/// The wet is turned down so it never pushes the output past this, nor past the track's own
/// recent peak before the duck, which falls away over `PEAK_RELEASE_MS`, nor past the ducked
/// track itself where that is louder still. So the chord fills the gap the duck opens without
/// standing out above the track.
const CEILING: f64 = 1.0;
const PEAK_RELEASE_MS: f32 = 1_000.0;
const LIMITER_RELEASE_MS: f32 = 80.0;
/// The chord rings the track's own notes, so it can line up with the track and make the sum
/// louder than the two apart, and where the track has little at the chord's pitches the chord
/// stays quiet. So the output is brought back to the ducked track's loudness, turned down by at
/// most this much, and up by no more than undoes `dry_gain`, so the track is never louder than Off.
const MIN_MAKEUP: f64 = 0.7;
/// On hits with gaps between them, such as a ride or a strummed guitar, the chord rings on through
/// the gaps and the output comes out up to about 1.3 dB louder than Off at 100%, while sustained
/// tracks come out a little quieter. So the makeup aims a touch under the ducked track as the chord
/// comes up, 0.3 dB at 100%, which keeps both within about 1 dB of Off.
const MAKEUP_TRIM: f64 = 0.17;
/// Frames between updates of the tuning and the tail.
const CONTROL_FRAMES: usize = 32;

/// The root of key `key` between A3 and G#4.
pub fn root_hz(key: u8) -> f64 {
    let midi = 57.0 + ((key as usize + 3) % 12) as f64;
    440.0 * ((midi - 69.0) / 12.0).exp2()
}

/// How far the dry track is turned down at `amount`, so that with the chord, which has nothing
/// to do with the track's phase, the output is about as loud as Off.
/// The share of the ducked track's power the makeup aims the output at.
fn makeup_aim(amount: f64) -> f64 {
    let wet = WET_LEVEL * amount.clamp(0.0, 1.0);
    1.0 - MAKEUP_TRIM * wet * wet
}

pub(crate) fn dry_gain(amount: f64) -> f64 {
    let wet = WET_LEVEL * amount.clamp(0.0, 1.0);
    (1.0 - wet * wet).sqrt()
}

/// How long after the track the resonators hear it, in milliseconds, for an Attack of
/// `attack_ms`.
pub(crate) fn excite_delay_ms(attack_ms: f64) -> f64 {
    attack_ms.clamp(0.0, LONGEST_ATTACK_MS) + EXCITE_SWELLS * SWELL_ATTACK_MS as f64
}

/// The track's mid before the duck, held long enough to be heard again after any delay
/// `excite_delay_ms` can ask for, and read back between frames.
#[derive(Debug, Clone, Default)]
struct DelayLine {
    buffer: Vec<f64>,
    write: usize,
}

impl DelayLine {
    /// Allocates for `frames` of delay. Not for the audio thread.
    fn prepare(&mut self, frames: usize) {
        self.buffer = vec![0.0; frames + 2];
        self.write = 0;
    }

    fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }

    fn push(&mut self, value: f64) {
        self.buffer[self.write] = value;
        self.write = (self.write + 1) % self.buffer.len();
    }

    /// The value pushed `frames` frames before the last one, linearly interpolated.
    fn read(&self, frames: f64) -> f64 {
        let length = self.buffer.len();
        let frames = frames.clamp(0.0, (length - 2) as f64);
        let whole = frames.floor();
        let fraction = frames - whole;
        let newest = (self.write + length - 1) % length;
        let at = (newest + length - whole as usize) % length;
        let before = (at + length - 1) % length;
        self.buffer[at] + fraction * (self.buffer[before] - self.buffer[at])
    }
}

/// A two-pole band-pass resonator with unit gain at its pitch: a topology-preserving state
/// variable filter, which stays stable however its tuning moves.
#[derive(Debug, Clone, Copy, Default)]
struct Resonator {
    ic1: f64,
    ic2: f64,
    a1: f64,
    a2: f64,
    a3: f64,
    k: f64,
}

impl Resonator {
    fn tune(&mut self, hz: f64, tail: f64, sample_rate: f64) {
        let hz = hz.min(sample_rate * 0.45);
        let g = (PI * hz / sample_rate).tan();
        // Poles decaying at k*pi*f per second put -60 dB at 6.9 / (k*pi*f) seconds.
        let k = (2.2 / (hz * tail)).min(1.0);
        self.a1 = 1.0 / (1.0 + g * (g + k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
        self.k = k;
    }

    fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }

    fn process(&mut self, input: f64) -> f64 {
        let v3 = input - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        self.k * v1
    }
}

/// One chord of voices tuned to one key: each partial of each voice, on the left and the right.
#[derive(Debug, Clone, Default)]
struct Bank {
    key: u8,
    voices: [[[Resonator; 2]; PARTIALS.len()]; VOICES],
    /// How much later than the other side the late side of each partial hears the track, in frames.
    offsets: [[f64; PARTIALS.len()]; VOICES],
}

impl Bank {
    /// `cents` is each voice's detune, which the left takes up and the right down.
    fn tune(&mut self, tail: f64, cents: &[f64; VOICES], sample_rate: f64) {
        let root = root_hz(self.key);
        for (voice, offsets) in self.offsets.iter_mut().enumerate() {
            for (offset, (ratio, _, _)) in offsets.iter_mut().zip(PARTIALS) {
                let hz = root * ratio * (VOICING[voice] / 12.0).exp2();
                let cycles = (hz * STEREO_OFFSET_MS * 0.001).floor() + 0.25;
                *offset = cycles / hz * sample_rate;
            }
        }
        for (voice, partials) in self.voices.iter_mut().enumerate() {
            for (pair, (ratio, _, share)) in partials.iter_mut().zip(PARTIALS) {
                for (side, resonator) in pair.iter_mut().enumerate() {
                    let detune = if side == 0 { cents[voice] } else { -cents[voice] };
                    let hz = root * ratio * ((VOICING[voice] * 100.0 + detune) / 1200.0).exp2();
                    resonator.tune(hz, tail * share, sample_rate);
                }
            }
        }
    }

    fn reset(&mut self) {
        self.voices.iter_mut().flatten().flatten().for_each(Resonator::reset);
    }
}

/// A 24 dB/oct high-pass then a gentle high shelf, on one side of the wet signal.
#[derive(Debug, Clone, Default)]
struct WetFilter {
    high_pass: [Svf; 2],
    air_low: f64,
    air_coefficient: f64,
}

impl WetFilter {
    fn set(&mut self, sample_rate: f64) {
        for stage in &mut self.high_pass {
            stage.set(WET_HIGH_PASS_HZ, sample_rate);
        }
        self.air_coefficient = 1.0 - (-TAU * AIR_HZ / sample_rate).exp();
    }

    fn reset(&mut self) {
        self.high_pass.iter_mut().for_each(Svf::reset);
        self.air_low = 0.0;
    }

    fn process(&mut self, input: f64) -> f64 {
        let clean = self.high_pass.iter_mut().fold(input, |value, stage| stage.process(value).1);
        self.air_low += self.air_coefficient * (clean - self.air_low);
        clean + (AIR_GAIN - 1.0) * (clean - self.air_low)
    }
}

/// A signal's mean power over a few hits, which still drops within a second when the signal
/// falls far below it, and is never far below the signal's power right now.
#[derive(Debug, Clone, Default)]
struct PowerMeter {
    slow: f64,
    fast: f64,
    /// The loudest `fast` in each of the last few blocks, and in the block being filled.
    blocks: [f64; POWER_BLOCKS],
    filling: f64,
    next_block: usize,
    countdown: usize,
}

impl PowerMeter {
    fn reset(&mut self) {
        *self = PowerMeter::default();
    }

    /// Adds one frame's power and returns the mean.
    fn update(&mut self, power: f64, slow_rate: f64, fast_rate: f64, block_frames: usize) -> f64 {
        let before = self.slow + slow_rate * (power - self.slow);
        self.fast += fast_rate * (power - self.fast);
        self.filling = self.filling.max(self.fast);
        if self.countdown == 0 {
            self.blocks[self.next_block] = self.filling;
            self.next_block = (self.next_block + 1) % POWER_BLOCKS;
            self.filling = self.fast;
            self.countdown = block_frames;
        }
        self.countdown -= 1;
        let recent_peak = self.blocks.iter().fold(self.filling, |peak, &block| peak.max(block));
        self.slow = before.min(recent_peak);
        self.slow.max(self.fast / POWER_SURGE)
    }
}

/// Equal-power gains for a pan from -1 to 1.
fn pan_gains(pan: f64) -> [f64; 2] {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * PI / 4.0;
    [angle.cos(), angle.sin()]
}

pub struct DreamEater {
    sample_rate: f64,
    detector: KeyDetector,
    choice: KeyChoice,
    /// Two banks, so a new key can take over while the old one rings out.
    banks: [Bank; 2],
    active: usize,
    /// 0..1, how far the active bank has taken over the input.
    crossfade: f64,
    crossfade_step: f64,
    since_retune: usize,
    retune_gap: usize,
    /// Whether the voices ran last frame. They start from silence each time Dream Eater comes on.
    running: bool,
    /// The sample's envelope, smoothed so the voices swell instead of jumping.
    swell: f64,
    swell_attack: f64,
    swell_release: f64,
    /// Each voice's detune LFO phase, in cycles.
    shimmer: [f64; VOICES],
    /// Each voice partial's level on the left and right.
    gains: [[[f64; 2]; PARTIALS.len()]; VOICES],
    countdown: usize,
    /// The track's mid before the duck, which the resonators hear `delay` frames late.
    excitation: DelayLine,
    /// The delay the Attack asks for, and the one in use, which glides to it.
    delay_target: f64,
    delay: f64,
    delay_glide: f64,
    wet_filters: [WetFilter; 2],
    /// The track's power after the duck, and the chord's before the leveller.
    dry_power: PowerMeter,
    wet_power: PowerMeter,
    /// The output's power before the makeup, and the makeup that brings it back to the ducked
    /// track's.
    out_power: PowerMeter,
    makeup: f64,
    leveller: f64,
    limiter: f64,
    /// The track's recent peak before the duck.
    track_peak: f64,
    peak_release: f64,
    loudness_rate: f64,
    surge_rate: f64,
    surge_block: usize,
    leveller_glide: f64,
    leveller_rise: f64,
    limiter_release: f64,
    #[cfg(test)]
    pub(crate) probe: Probe,
}

/// Running sums of what happens inside, for reports on real material.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct Probe {
    pub frames: f64,
    pub mid_power: f64,
    pub delayed_power: f64,
    pub input_power: f64,
    pub open: f64,
    pub raw_wet_power: f64,
    pub ducked_power: f64,
    pub leveller: f64,
    pub capped: f64,
    pub makeup: f64,
    pub limiter: f64,
    pub wet_out_power: f64,
}

impl DreamEater {
    pub fn new(sample_rate: f64) -> Self {
        let mut gains = [[[0.0; 2]; PARTIALS.len()]; VOICES];
        for (voice, partials) in gains.iter_mut().enumerate() {
            for (partial, gain) in partials.iter_mut().enumerate() {
                *gain = pan_gains(PAN[voice][partial]).map(|side| side * PARTIALS[partial].1);
            }
        }
        let mut dream = DreamEater {
            sample_rate,
            detector: KeyDetector::default(),
            choice: KeyChoice::Auto,
            banks: Default::default(),
            active: 0,
            crossfade: 1.0,
            crossfade_step: 0.0,
            since_retune: 0,
            retune_gap: 0,
            running: false,
            swell: 0.0,
            swell_attack: 0.0,
            swell_release: 0.0,
            shimmer: [0.0; VOICES],
            gains,
            countdown: 0,
            excitation: DelayLine::default(),
            delay_target: 0.0,
            delay: 0.0,
            delay_glide: 0.0,
            wet_filters: Default::default(),
            dry_power: PowerMeter::default(),
            wet_power: PowerMeter::default(),
            out_power: PowerMeter::default(),
            makeup: 1.0,
            leveller: 0.0,
            limiter: 1.0,
            track_peak: 0.0,
            peak_release: 0.0,
            loudness_rate: 0.0,
            surge_rate: 0.0,
            surge_block: 1,
            leveller_glide: 0.0,
            leveller_rise: 1.0,
            limiter_release: 0.0,
            #[cfg(test)]
            probe: Probe::default(),
        };
        dream.prepare(sample_rate);
        dream
    }

    /// Sets the sample rate and allocates for it. Not for the audio thread.
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.detector.prepare(sample_rate);
        self.crossfade_step = 1.0 / (CROSSFADE_SECONDS * sample_rate);
        self.retune_gap = (RETUNE_GAP_SECONDS * sample_rate) as usize;
        self.swell_attack = 1.0 - one_pole(SWELL_ATTACK_MS, sample_rate);
        self.swell_release = 1.0 - one_pole(SWELL_RELEASE_MS, sample_rate);
        let longest = (excite_delay_ms(LONGEST_ATTACK_MS) + STEREO_ROOM_MS) * 0.001 * sample_rate;
        self.excitation.prepare(longest.ceil() as usize);
        self.delay_glide = 1.0 - one_pole(DELAY_GLIDE_MS, sample_rate);
        self.set_attack(crate::dsp::Settings::default().attack_ms);
        self.delay = self.delay_target;
        for filter in &mut self.wet_filters {
            filter.set(sample_rate);
        }
        self.loudness_rate = 1.0 - one_pole(LOUDNESS_MS, sample_rate);
        self.surge_rate = 1.0 - one_pole(SURGE_MS, sample_rate);
        self.surge_block = ((SURGE_BLOCK_MS * 0.001 * sample_rate) as usize).max(1);
        self.leveller_glide = 1.0 - one_pole(10.0, sample_rate);
        self.leveller_rise = 10f64.powf(LEVELLER_RISE_DB_PER_SECOND / 20.0 / sample_rate);
        self.limiter_release = 1.0 - one_pole(LIMITER_RELEASE_MS, sample_rate);
        self.peak_release = 1.0 - one_pole(PEAK_RELEASE_MS, sample_rate);
        self.reset();
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        self.excitation.reset();
        self.delay = self.delay_target;
        self.running = false;
        self.swell = 0.0;
    }

    /// Follows the ducking's Attack, which sets how late the resonators hear the track.
    pub fn set_attack(&mut self, attack_ms: f32) {
        self.delay_target = excite_delay_ms(attack_ms as f64) * 0.001 * self.sample_rate;
    }

    /// How late the resonators hear the track, in frames, once any change of Attack has settled.
    #[cfg(test)]
    pub(crate) fn delay_frames(&self) -> f64 {
        self.delay_target
    }

    pub fn choose(&mut self, choice: KeyChoice) {
        self.choice = choice;
    }

    pub fn detected_key(&self) -> Option<u8> {
        self.detector.key()
    }

    /// The key the voices are tuned to, or will be once Dream Eater is on.
    pub fn key(&self) -> u8 {
        if self.running {
            self.banks[self.active].key
        } else {
            self.target_key()
        }
    }

    /// 0..1, how open the voices are to the track right now. 0 while Dream Eater is off.
    pub fn level(&self) -> f32 {
        if self.running {
            self.swell.clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }

    fn target_key(&self) -> u8 {
        match self.choice {
            KeyChoice::Fixed(key) => key % 12,
            KeyChoice::Auto => self.detector.key().unwrap_or(FALLBACK_KEY),
        }
    }

    fn start(&mut self) {
        let key = self.target_key();
        for bank in &mut self.banks {
            bank.key = key;
            bank.reset();
        }
        self.active = 0;
        self.crossfade = 1.0;
        self.since_retune = self.retune_gap;
        self.shimmer = [0.0, 0.37, 0.71];
        for filter in &mut self.wet_filters {
            filter.reset();
        }
        self.dry_power.reset();
        self.wet_power.reset();
        self.out_power.reset();
        self.makeup = 1.0;
        self.leveller = 0.0;
        self.limiter = 1.0;
        self.track_peak = 0.0;
        self.countdown = 0;
        self.running = true;
    }

    /// Retunes to a new key when it is time, and updates the tuning, shimmer, and tail.
    fn control(&mut self, amount: f64) {
        let target = self.target_key();
        if target != self.banks[self.active].key && self.crossfade >= 1.0 && self.since_retune >= self.retune_gap {
            self.active = 1 - self.active;
            self.banks[self.active].key = target;
            self.crossfade = 0.0;
            self.since_retune = 0;
        }
        self.since_retune = self.since_retune.saturating_add(CONTROL_FRAMES);
        let tail = TAIL_SHORTEST + (TAIL_LONGEST - TAIL_SHORTEST) * amount;
        let mut cents = [0.0; VOICES];
        for (voice, phase) in self.shimmer.iter_mut().enumerate() {
            *phase = (*phase + SHIMMER_HZ[voice] * CONTROL_FRAMES as f64 / self.sample_rate).fract();
            cents[voice] = SHIMMER_CENTS * (TAU * *phase).sin();
        }
        for bank in &mut self.banks {
            bank.tune(tail, &cents, self.sample_rate);
        }
    }

    /// Listens to one frame of the track and, unless `amount` is Off, returns it ducked by
    /// `heard` with the chord mixed in after the duck. `channels` is 1 or 2; with 1, only
    /// `track[0]` is used. `envelope` is the sample's normalised envelope.
    pub fn process(
        &mut self,
        track: [f32; 2],
        channels: usize,
        envelope: f32,
        heard: f32,
        amount: f32,
    ) -> Option<[f32; 2]> {
        let stereo = channels > 1;
        let mid = if stereo { 0.5 * (track[0] + track[1]) } else { track[0] };
        self.detector.push(mid);
        self.excitation.push(mid as f64);
        self.delay += self.delay_glide * (self.delay_target - self.delay);
        let envelope = envelope as f64;
        let rate = if envelope > self.swell { self.swell_attack } else { self.swell_release };
        self.swell += rate * (envelope - self.swell);
        if amount.is_nan() || amount <= 0.0 {
            self.running = false;
            return None;
        }
        if !self.running {
            self.start();
        }
        let amount = amount.min(1.0) as f64;
        if self.countdown == 0 {
            self.control(amount);
            self.countdown = CONTROL_FRAMES;
        }
        self.countdown -= 1;
        self.crossfade = (self.crossfade + self.crossfade_step).min(1.0);

        // The track before the duck, late enough to land in the gap the duck opens. Squared, so
        // the voices open mostly on the hit itself.
        let delayed = self.excitation.read(self.delay);
        let open = RESTING_OPEN + (1.0 - RESTING_OPEN) * self.swell.clamp(0.0, 1.0).powi(2);
        let mut wet = [0.0f64; 2];
        for (index, bank) in self.banks.iter_mut().enumerate() {
            let share = open * if index == self.active { self.crossfade } else { 1.0 - self.crossfade };
            for (voice, (partials, gains)) in bank.voices.iter_mut().zip(&self.gains).enumerate() {
                for (partial, (pair, gain)) in partials.iter_mut().zip(gains).enumerate() {
                    // The late side alternates, so neither side leads the whole chord.
                    let late_side = (voice + partial) % 2;
                    let late = self.excitation.read(self.delay + bank.offsets[voice][partial]);
                    for side in 0..2 {
                        let hears = if side == late_side { late } else { delayed };
                        let ring = pair[side].process(share * hears);
                        wet[side] += ring * gain[side];
                    }
                }
            }
        }
        for (side, filter) in wet.iter_mut().zip(&mut self.wet_filters) {
            *side = filter.process(*side);
        }
        let heard = heard as f64;
        let track = if stereo { [track[0] as f64, track[1] as f64] } else { [track[0] as f64; 2] };
        if !wet.iter().all(|side| side.is_finite()) {
            self.start();
            return Some(track.map(|side| (heard * side) as f32));
        }
        let wet = if stereo { wet } else { [0.5 * (wet[0] + wet[1]); 2] };

        // The leveller holds the chord's mean power at `WET_LEVEL` squared times the ducked
        // track's, judged over several hits so each hit's swell comes through.
        let (rate, surge_rate, block) = (self.loudness_rate, self.surge_rate, self.surge_block);
        let ducked_power = heard * heard * (track[0] * track[0] + track[1] * track[1]);
        let dry_power = self.dry_power.update(ducked_power, rate, surge_rate, block);
        let wet_power = self.wet_power.update(wet[0] * wet[0] + wet[1] * wet[1], rate, surge_rate, block);
        let target = (WET_LEVEL * amount * (dry_power / wet_power.max(1e-30)).sqrt()).min(MAX_GAIN);
        let glided = self.leveller + self.leveller_glide * (target - self.leveller);
        self.leveller = glided.min(self.leveller.max(1.0) * self.leveller_rise);
        let dry = track.map(|side| heard * dry_gain(amount) * side);
        let wet = wet.map(|side| self.leveller * side);
        // Brings the sum back to the ducked track's loudness, where the chord lines up with the
        // track or finds little in it to ring.
        let sum_power = self.out_power.update(
            (dry[0] + wet[0]).powi(2) + (dry[1] + wet[1]).powi(2),
            rate,
            surge_rate,
            block,
        );
        let most = 1.0 / dry_gain(amount);
        let aim = dry_power * makeup_aim(amount);
        let makeup = if sum_power > 1e-30 { (aim / sum_power).sqrt().clamp(MIN_MAKEUP, most) } else { 1.0 };
        self.makeup += self.leveller_glide * (makeup - self.makeup);
        let (dry, wet) = (dry.map(|side| self.makeup * side), wet.map(|side| self.makeup * side));
        // Turns the wet down, never the track, so the output stays under the ceiling or the
        // track's own peak. The track itself is never above its level at Off.
        let loudest = track[0].abs().max(track[1].abs());
        self.track_peak = loudest.max(self.track_peak * (1.0 - self.peak_release));
        let room = self.track_peak.min(CEILING);
        let mut fits = 1.0f64;
        for side in 0..2 {
            let limit = dry[side].abs().max(room);
            let sum = dry[side] + wet[side];
            if sum.abs() > limit {
                fits = fits.min(((limit * sum.signum() - dry[side]) / wet[side]).clamp(0.0, 1.0));
            }
        }
        self.limiter = fits.min(self.limiter + self.limiter_release * (1.0 - self.limiter));
        #[cfg(test)]
        {
            let p = &mut self.probe;
            p.frames += 1.0;
            p.mid_power += (mid as f64).powi(2);
            p.delayed_power += delayed * delayed;
            p.input_power += (open * delayed).powi(2);
            p.open += open;
            p.raw_wet_power += (wet[0] * wet[0] + wet[1] * wet[1]) / (self.leveller * self.makeup).powi(2).max(1e-30);
            p.ducked_power += ducked_power;
            p.leveller += self.leveller;
            p.capped += if target >= MAX_GAIN * 0.999 { 1.0 } else { 0.0 };
            p.makeup += self.makeup;
            p.limiter += self.limiter;
            p.wet_out_power += (self.limiter * self.limiter) * (wet[0] * wet[0] + wet[1] * wet[1]);
        }
        Some([0, 1].map(|side| (dry[side] + self.limiter * wet[side]) as f32))
    }

    #[cfg(test)]
    pub(crate) fn leveller(&self) -> f64 {
        self.leveller
    }

    #[cfg(test)]
    pub(crate) fn makeup(&self) -> f64 {
        self.makeup
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    fn midi_hz(note: u8) -> f64 {
        440.0 * ((note as f64 - 69.0) / 12.0).exp2()
    }

    /// Chords as MIDI notes, each `seconds` long, played as plucked tones with a few harmonics,
    /// round and round for `total` seconds.
    fn progression(chords: &[&[u8]], seconds: f64, total: f64) -> Vec<f32> {
        let frames = (total * RATE) as usize;
        let per_chord = (seconds * RATE) as usize;
        (0..frames)
            .map(|n| {
                let chord = chords[(n / per_chord) % chords.len()];
                let t = (n % per_chord) as f64 / RATE;
                let pluck = (1.0 - (-t * 200.0).exp()) * (-t * 2.0).exp();
                let value: f64 = chord
                    .iter()
                    .map(|&note| {
                        let hz = midi_hz(note);
                        (1..=4).map(|h| (TAU * hz * h as f64 * n as f64 / RATE).sin() / h as f64).sum::<f64>()
                    })
                    .sum();
                (0.08 * pluck * value) as f32
            })
            .collect()
    }

    /// i-VI-III-VII: Am F C G.
    const A_MINOR: &[&[u8]] = &[&[45, 57, 60, 64], &[41, 57, 60, 65], &[48, 55, 60, 64], &[43, 55, 59, 62]];
    /// I-V-vi-IV: D A Bm G.
    const D_MAJOR: &[&[u8]] = &[&[38, 50, 54, 57], &[45, 57, 61, 64], &[47, 54, 59, 62], &[43, 55, 59, 62]];
    /// Other common orders, so the detection doesn't hang on one progression.
    const A_MINOR_AGAIN: &[&[u8]] = &[&[45, 57, 60, 64], &[50, 57, 62, 65], &[40, 55, 59, 64], &[45, 57, 60, 64]];
    const D_MAJOR_AGAIN: &[&[u8]] = &[&[38, 50, 54, 57], &[43, 55, 59, 62], &[45, 57, 61, 64], &[38, 50, 54, 57]];

    fn noise(frames: usize, seed: u32) -> Vec<f32> {
        let mut seed = seed;
        (0..frames)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
            })
            .collect()
    }

    fn detector() -> KeyDetector {
        let mut detector = KeyDetector::default();
        detector.prepare(RATE);
        detector
    }

    fn listen(detector: &mut KeyDetector, audio: &[f32]) {
        audio.iter().for_each(|&v| detector.push(v));
    }

    #[test]
    #[ignore]
    fn key_report() {
        for (name, audio) in [
            ("A minor", progression(A_MINOR, 0.5, 8.0)),
            ("D major", progression(D_MAJOR, 0.5, 8.0)),
            ("A minor i-iv-v-i", progression(A_MINOR_AGAIN, 0.5, 8.0)),
            ("D major I-IV-V-I", progression(D_MAJOR_AGAIN, 0.5, 8.0)),
            ("noise", noise(6 * RATE as usize, 7)),
        ] {
            let mut detector = detector();
            println!("{name}:");
            for chunk in audio.chunks(RATE as usize) {
                listen(&mut detector, chunk);
                let fits = detector.fits().map(|v| (v * 100.0).round() as i32);
                println!("  key {:?} heard {:.1} fits {fits:?}", detector.key(), detector.heard);
            }
            println!("  chroma {:?}", detector.chroma.map(|v| (v * 100.0).round() as i32));
        }
    }

    #[test]
    fn the_fft_finds_a_tone_in_its_bin() {
        let n = 64;
        let mut re: Vec<f32> = (0..n).map(|i| (TAU * 5.0 * i as f64 / n as f64).cos() as f32).collect();
        let mut im = vec![0.0; n];
        let cos: Vec<f32> = (0..n / 2).map(|k| (TAU * k as f64 / n as f64).cos() as f32).collect();
        let sin: Vec<f32> = (0..n / 2).map(|k| (TAU * k as f64 / n as f64).sin() as f32).collect();
        fft(&mut re, &mut im, &cos, &sin);
        for k in 0..n {
            let magnitude = re[k].hypot(im[k]);
            let expected = if k == 5 || k == n - 5 { n as f32 / 2.0 } else { 0.0 };
            assert!((magnitude - expected).abs() < 1e-3, "bin {k}: {magnitude}");
        }
    }

    #[test]
    fn hears_a_minor_as_the_c_pentatonic() {
        for chords in [A_MINOR, A_MINOR_AGAIN] {
            let mut detector = detector();
            listen(&mut detector, &progression(chords, 0.5, 8.0));
            assert_eq!(detector.key(), Some(0), "A minor shares C major's pentatonic");
        }
    }

    #[test]
    fn hears_d_major() {
        for chords in [D_MAJOR, D_MAJOR_AGAIN] {
            let mut detector = detector();
            listen(&mut detector, &progression(chords, 0.5, 8.0));
            assert_eq!(detector.key(), Some(2));
        }
    }

    #[test]
    fn hears_keys_at_other_sample_rates() {
        for rate in [44_100.0, 96_000.0] {
            let mut detector = KeyDetector::default();
            detector.prepare(rate);
            let audio = progression(D_MAJOR, 0.5, 8.0);
            // Resampled by holding frames, which is plenty for a key.
            let frames = (audio.len() as f64 * rate / RATE) as usize;
            for n in 0..frames {
                detector.push(audio[((n as f64 * RATE / rate) as usize).min(audio.len() - 1)]);
            }
            assert_eq!(detector.key(), Some(2), "at {rate} Hz");
        }
    }

    #[test]
    fn noise_and_silence_have_no_key_and_a_heard_key_is_held() {
        let mut detector = detector();
        listen(&mut detector, &noise(6 * RATE as usize, 7));
        assert_eq!(detector.key(), None, "noise has no key");
        listen(&mut detector, &vec![0.0; 2 * RATE as usize]);
        assert_eq!(detector.key(), None);

        listen(&mut detector, &progression(A_MINOR, 0.5, 8.0));
        assert_eq!(detector.key(), Some(0));
        let mut drums = noise(6 * RATE as usize, 11);
        drums.iter_mut().enumerate().for_each(|(n, v)| *v *= (-((n % 24_000) as f32 / 2_400.0)).exp());
        listen(&mut detector, &drums);
        listen(&mut detector, &vec![0.0; 2 * RATE as usize]);
        assert_eq!(detector.key(), Some(0), "noise and silence keep the last confident key");
    }

    #[test]
    fn a_key_change_waits_until_it_is_clear() {
        let mut detector = detector();
        listen(&mut detector, &progression(A_MINOR, 0.5, 8.0));
        assert_eq!(detector.key(), Some(0));
        let d_major = progression(D_MAJOR, 0.5, 10.0);
        listen(&mut detector, &d_major[..RATE as usize]);
        assert_eq!(detector.key(), Some(0), "one second of a new key isn't enough");
        listen(&mut detector, &d_major[RATE as usize..]);
        assert_eq!(detector.key(), Some(2), "but it moves once the new key is clear");
    }

    #[test]
    fn key_choices_round_trip_through_their_ids() {
        assert_eq!(KeyChoice::all().count(), 13);
        for choice in KeyChoice::all() {
            let json = serde_json::to_string(&choice).unwrap();
            assert_eq!(serde_json::from_str::<KeyChoice>(&json).unwrap(), choice, "{json}");
            assert_eq!(KeyChoice::from_bits(choice.to_bits()), choice);
        }
        assert_eq!(serde_json::to_string(&KeyChoice::Auto).unwrap(), r#""auto""#);
        assert_eq!(serde_json::to_string(&KeyChoice::Fixed(1)).unwrap(), r#""c#""#);
        assert!(serde_json::from_str::<KeyChoice>(r#""h""#).is_err());
        assert_eq!(KeyChoice::Fixed(0).label(), "C major / A minor");
        assert_eq!(KeyChoice::Fixed(3).label(), "D#/Eb major / C minor");
        assert_eq!(KeyChoice::Fixed(1).label(), "C#/Db major / A#/Bb minor");
    }

    #[test]
    fn roots_sit_between_a3_and_g_sharp_4() {
        assert!((root_hz(9) - 220.0).abs() < 1e-9);
        assert!((root_hz(0) - 261.626).abs() < 1e-3);
        assert!((root_hz(8) - 415.305).abs() < 1e-3);
        for key in 0..12 {
            let root = root_hz(key);
            let top = root * (VOICING[2] / 12.0).exp2() * PARTIALS[1].0;
            assert!(root >= 200.0 && top < 2_000.0, "key {key}: {root} to {top}");
        }
    }

    /// Goertzel power of `signal` at `hz`.
    fn power_at(signal: &[f32], hz: f64) -> f64 {
        let coefficient = 2.0 * (TAU * hz / RATE).cos();
        let (mut s1, mut s2) = (0.0, 0.0);
        for &v in signal {
            let s0 = v as f64 + coefficient * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coefficient * s1 * s2) / (signal.len() as f64).powi(2)
    }

    fn rms(signal: &[f32]) -> f32 {
        (signal.iter().map(|v| v * v).sum::<f32>() / signal.len().max(1) as f32).sqrt()
    }

    #[test]
    fn retuning_crossfades_without_a_click() {
        let mut dream = DreamEater::new(RATE);
        dream.choose(KeyChoice::Fixed(0));
        let input = noise(6 * RATE as usize, 3).iter().map(|v| v * 0.3).collect::<Vec<f32>>();
        let switch = 2 * RATE as usize;
        let mut wet = Vec::with_capacity(input.len());
        for (n, &v) in input.iter().enumerate() {
            if n == switch {
                dream.choose(KeyChoice::Fixed(6));
            }
            // A hit every half second.
            let envelope = (-((n % 24_000) as f32) / 4_800.0).exp();
            let out = dream.process([v, v], 2, envelope, 1.0, 1.0).unwrap();
            // The chord alone, without the track under it.
            wet.push(out[0] - (dream.makeup() * dry_gain(1.0)) as f32 * v);
        }
        assert_eq!(dream.key(), 6);

        let (c, f_sharp) = (root_hz(0), root_hz(6));
        let before = &wet[RATE as usize..switch];
        let after = &wet[switch + 2 * RATE as usize..];
        assert!(power_at(before, c) > 4.0 * power_at(before, f_sharp), "rings on C first");
        assert!(power_at(after, f_sharp) > 4.0 * power_at(after, c), "then on F#");

        // No jump in the wet: its 5 ms RMS moves no faster across the switch than with the hits.
        let windows: Vec<f32> = wet.chunks(240).map(rms).collect();
        let jump = |range: std::ops::Range<usize>| {
            windows[range].windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0f32, f32::max)
        };
        let per_second = windows.len() / 6;
        let steady = jump(per_second..2 * per_second);
        let across = jump(2 * per_second - 4..3 * per_second);
        assert!(across < steady * 1.5, "{across} vs {steady}");
        // Nor a click: no step between samples bigger than with the hits, for the wet's level.
        // The track is noise, which has more to ring at some keys than others.
        let step = |range: std::ops::Range<usize>| {
            let level = rms(&wet[range.clone()]);
            wet[range].windows(2).map(|pair| (pair[1] - pair[0]).abs()).fold(0.0f32, f32::max) / level
        };
        let (steady_step, switch_step) = (step(RATE as usize..switch), step(switch - 480..switch + RATE as usize));
        assert!(switch_step < steady_step * 1.5, "{switch_step} vs {steady_step}");
    }

    #[test]
    fn silence_in_is_silence_out_with_no_hiss() {
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            let mut dream = DreamEater::new(rate);
            let frames = 4 * rate as usize;
            for n in 0..frames {
                // A hit every half second, fully open on each.
                let envelope = (-((n % (rate as usize / 2)) as f32) / (0.1 * rate as f32)).exp();
                let out = dream.process([0.0, 0.0], 2, envelope, 1.0 - 0.75 * envelope, 1.0).unwrap();
                assert_eq!(out, [0.0, 0.0], "at {rate} Hz, frame {n}");
            }
            assert!(dream.level() > 0.0, "the voices were open");
        }
    }

    #[test]
    fn the_resonators_hear_the_track_late_by_the_attack_and_swell() {
        assert_eq!(excite_delay_ms(1.0), 51.0);
        assert_eq!(excite_delay_ms(100.0), 150.0);
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            for attack in [0.1f32, 1.0, 30.0, 100.0] {
                let mut dream = DreamEater::new(rate);
                dream.choose(KeyChoice::Fixed(0));
                dream.set_attack(attack);
                dream.reset();
                let delay = excite_delay_ms(attack as f64) * 0.001 * rate;
                assert!((dream.delay_frames() - delay).abs() < 1e-9);
                // One click, with the voices fully open throughout.
                let mut wet = Vec::new();
                for n in 0..(delay as usize + rate as usize / 10) {
                    let v = if n == 100 { 1.0 } else { 0.0 };
                    let out = dream.process([v, v], 2, 1.0, 1.0, 1.0).unwrap();
                    // The click itself passes straight through, so it is left out.
                    wet.push(if n == 100 { 0.0 } else { out[0] as f64 });
                }
                let first = wet.iter().position(|v| v.abs() > 1e-12).unwrap();
                assert!(
                    (first as f64 - (100.0 + delay)).abs() <= 1.0,
                    "at {rate} Hz, Attack {attack}: rings from frame {first}, expected {}",
                    100.0 + delay
                );
            }
        }
    }

    #[test]
    fn a_new_key_waits_for_the_old_one_to_ring_out() {
        let mut dream = DreamEater::new(RATE);
        dream.choose(KeyChoice::Fixed(0));
        let input = noise(RATE as usize * 4, 5);
        let mut keys = Vec::new();
        for (n, &v) in input.iter().enumerate() {
            match n {
                24_000 => dream.choose(KeyChoice::Fixed(7)),
                48_000 => dream.choose(KeyChoice::Fixed(2)),
                _ => {}
            }
            dream.process([v, v], 2, 0.5, 1.0, 1.0);
            keys.push(dream.key());
        }
        assert_eq!(keys[24_100], 7, "the first change is immediate");
        assert_eq!(keys[60_000], 7, "the second waits for the gap");
        assert_eq!(keys[24_000 + (RETUNE_GAP_SECONDS * RATE) as usize + 100], 2);
    }
}
