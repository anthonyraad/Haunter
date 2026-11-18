//! Dream Eater's spectral gate, the same one Hyperchroma's Splatter knob runs.
//!
//! One Hann STFT. Left and right are transformed separately and share a gate
//! decision, so the stereo image holds still. The knob sets how far below the
//! frame's loudest partial a bin has to sit before it is thinned, and how far
//! a closed bin is pushed down.



use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

const KNEE_DB: f32 = 6.0;
const RAMP_KNOB: f32 = 0.05;
const ATTACK_SEC: f32 = 0.010;
const RELEASE_SEC: f32 = 0.120;
const KNOB_SEC: f32 = 0.020;

pub struct SpectralGate {
    sample_rate: f32,
    frame: usize,
    hop: usize,
    /// Analysis window, already scaled by `1 / (frame * cola)`.
    window: Vec<f32>,
    latency: u32,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    scratch: Vec<Complex32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    wet_l: Vec<f32>,
    wet_r: Vec<f32>,
    time_l: Vec<f32>,
    time_r: Vec<f32>,
    spec_l: Vec<Complex32>,
    spec_r: Vec<Complex32>,
    gains: Vec<f32>,
    mags: Vec<f32>,
    pos: usize,
    knob: f32,
    knob_coeff: f32,
    attack_coeff: f32,
    release_coeff: f32,
    /// Rises over one frame after the effect leaves bypass, so a cold STFT fades in.
    warm: f32,
    idle: bool,
    /// Samples of already-buffered audio still waiting to be emitted.
    /// Hyperchroma uses this to keep a voice alive; Haunter's insert always runs.
    #[allow(dead_code)]
    tail: u32,
    /// How many samples have been written into the delay rings since the last reset.
    filled: usize,
}

impl SpectralGate {
    pub fn new(sample_rate: f32) -> Self {
        let mut gate = Self {
            sample_rate: 0.0,
            frame: 0,
            hop: 1,
            window: Vec::new(),
            latency: 0,
            forward: RealFftPlanner::<f32>::new().plan_fft_forward(2048),
            inverse: RealFftPlanner::<f32>::new().plan_fft_inverse(2048),
            scratch: Vec::new(),
            out_l: Vec::new(),
            out_r: Vec::new(),
            dry_l: Vec::new(),
            dry_r: Vec::new(),
            wet_l: Vec::new(),
            wet_r: Vec::new(),
            time_l: Vec::new(),
            time_r: Vec::new(),
            spec_l: Vec::new(),
            spec_r: Vec::new(),
            gains: Vec::new(),
            mags: Vec::new(),
            pos: 0,
            knob: 0.0,
            knob_coeff: 1.0,
            attack_coeff: 1.0,
            release_coeff: 1.0,
            warm: 0.0,
            idle: true,
            tail: 0,
            filled: 0,
        };
        gate.set_sample_rate(sample_rate);
        gate
    }

    /// Samples the output lags the input while the gate is in the chain.
    pub fn hop_samples(&self) -> usize {
        self.hop
    }

    /// The smoothed knob, 0..1.
    pub fn amount(&self) -> f32 {
        self.knob
    }

    /// True once the delay rings hold a full frame, so engaging does not start from silence.
    pub fn primed(&self) -> bool {
        self.frame > 0 && self.filled >= self.frame
    }

    pub fn latency_samples(&self) -> u32 {
        self.latency
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        let sample_rate = if sample_rate.is_finite() && sample_rate > 1.0 {
            sample_rate
        } else {
            44_100.0
        };
        let frame = frame_samples(sample_rate);
        if (self.sample_rate - sample_rate).abs() < 0.1
            && self.frame == frame
            && !self.window.is_empty()
        {
            return;
        }
        self.sample_rate = sample_rate;
        self.frame = frame;
        self.hop = (frame / 4).max(1);
        self.latency = frame as u32;

        let mut planner = RealFftPlanner::<f32>::new();
        self.forward = planner.plan_fft_forward(frame);
        self.inverse = planner.plan_fft_inverse(frame);
        let scratch_len = self
            .forward
            .get_scratch_len()
            .max(self.inverse.get_scratch_len());
        self.scratch = vec![Complex32::default(); scratch_len];
        self.spec_l = self.forward.make_output_vec();
        self.spec_r = self.forward.make_output_vec();
        self.time_l = vec![0.0; frame];
        self.time_r = vec![0.0; frame];
        self.out_l = vec![0.0; frame];
        self.out_r = vec![0.0; frame];
        self.dry_l = vec![0.0; frame];
        self.dry_r = vec![0.0; frame];
        self.wet_l = vec![0.0; frame];
        self.wet_r = vec![0.0; frame];
        let bins = self.spec_l.len();
        self.gains = vec![1.0; bins];
        self.mags = vec![0.0; bins];

        let cola = cola_sum(frame, self.hop);
        let scale = 1.0 / (frame as f32 * cola);
        self.window = (0..frame)
            .map(|i| {
                let phase = i as f32 / frame as f32;
                (0.5 - 0.5 * (std::f32::consts::TAU * phase).cos()) * scale
            })
            .collect();

        let hop_sec = self.hop as f32 / sample_rate;
        self.attack_coeff = one_pole_coeff(hop_sec, ATTACK_SEC);
        self.release_coeff = one_pole_coeff(hop_sec, RELEASE_SEC);
        self.knob_coeff = one_pole_coeff(1.0 / sample_rate, KNOB_SEC);
        self.reset();
    }

    pub fn reset(&mut self) {
        for buf in [
            &mut self.out_l,
            &mut self.out_r,
            &mut self.dry_l,
            &mut self.dry_r,
            &mut self.wet_l,
            &mut self.wet_r,
            &mut self.time_l,
            &mut self.time_r,
        ] {
            buf.fill(0.0);
        }
        self.spec_l.fill(Complex32::default());
        self.spec_r.fill(Complex32::default());
        self.gains.fill(1.0);
        self.mags.fill(0.0);
        self.pos = 0;
        self.knob = 0.0;
        self.warm = 0.0;
        self.idle = true;
        self.tail = 0;
        self.filled = 0;
    }

    /// Writes one block into the delay rings without reading them and without an FFT.
    /// Off uses this so the rings stay current and turning the knob up does not open onto silence.
    pub fn feed(&mut self, wet_l: &[f32], wet_r: &[f32], dry_l: &[f32], dry_r: &[f32]) {
        let n = wet_l.len().min(wet_r.len()).min(dry_l.len()).min(dry_r.len());
        if n == 0 || self.frame == 0 {
            return;
        }
        for i in 0..n {
            self.dry_l[self.pos] = finite(dry_l[i]);
            self.dry_r[self.pos] = finite(dry_r[i]);
            self.wet_l[self.pos] = finite(wet_l[i]);
            self.wet_r[self.pos] = finite(wet_r[i]);
            self.pos += 1;
            if self.pos == self.frame {
                self.pos = 0;
            }
        }
        self.filled = (self.filled + n).min(self.frame);
    }

    /// Gate `wet` and mix it with the delayed `dry` bus.
    /// The final stereo mix is written back into `wet_l` / `wet_r`.
    /// `amount` is the Splatter knob for this block, before smoothing.
    /// The plugin calls `process_varying`; the constant-amount path is what the unit tests drive.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn process_block(
        &mut self,
        wet_l: &mut [f32],
        wet_r: &mut [f32],
        dry_l: &[f32],
        dry_r: &[f32],
        amount: f32,
    ) {
        self.process_with(wet_l, wet_r, dry_l, dry_r, |_| amount);
    }

    /// Like `process_block`, with a knob target for each sample.
    /// When the block ends in bypass, the returned index is the first sample of that bypass.
    pub fn process_varying(
        &mut self,
        wet_l: &mut [f32],
        wet_r: &mut [f32],
        dry_l: &[f32],
        dry_r: &[f32],
        amounts: &[f32],
    ) -> Option<usize> {
        self.process_with(wet_l, wet_r, dry_l, dry_r, |i| amounts.get(i).copied().unwrap_or(0.0))
    }

    fn process_with(
        &mut self,
        wet_l: &mut [f32],
        wet_r: &mut [f32],
        dry_l: &[f32],
        dry_r: &[f32],
        mut amount_at: impl FnMut(usize) -> f32,
    ) -> Option<usize> {
        let n = wet_l
            .len()
            .min(wet_r.len())
            .min(dry_l.len())
            .min(dry_r.len());
        if n == 0 || self.frame == 0 {
            return None;
        }
        let mut peak = 0.0f32;
        // First sample of the bypass stretch the block ends in, if it ends bypassed.
        let mut closed_at: Option<usize> = None;
        for i in 0..n {
            let amount = amount_at(i);
            let amount = if amount.is_finite() {
                amount.clamp(0.0, 1.0)
            } else {
                0.0
            };
            self.smooth_knob(amount);
            let entering = self.idle && (amount > 1.0e-5 || self.knob > 1.0e-5);
            let was_idle = self.idle;
            if self.knob <= 1.0e-5 && amount <= 1.0e-5 {
                if !self.idle {
                    self.enter_bypass();
                }
                if closed_at.is_none() || !was_idle {
                    closed_at = Some(i);
                }
            } else if entering || self.idle {
                self.idle = false;
                self.warm = 0.0;
                self.gains.fill(1.0);
                closed_at = None;
            } else {
                closed_at = None;
            }

            let delayed_dry = self.dry_l[self.pos];
            let delayed_dry_r = self.dry_r[self.pos];
            let delayed_wet = self.wet_l[self.pos];
            let delayed_wet_r = self.wet_r[self.pos];
            let stft_l = self.out_l[self.pos];
            let stft_r = self.out_r[self.pos];
            self.dry_l[self.pos] = finite(dry_l[i]);
            self.dry_r[self.pos] = finite(dry_r[i]);
            self.wet_l[self.pos] = finite(wet_l[i]);
            self.wet_r[self.pos] = finite(wet_r[i]);
            self.out_l[self.pos] = 0.0;
            self.out_r[self.pos] = 0.0;

            peak = peak
                .max(self.dry_l[self.pos].abs())
                .max(self.dry_r[self.pos].abs())
                .max(self.wet_l[self.pos].abs())
                .max(self.wet_r[self.pos].abs());

            self.pos += 1;
            if self.pos == self.frame {
                self.pos = 0;
            }
            if !self.idle && self.pos % self.hop == 0 {
                self.process_hop();
            }

            let blend = wet_mix(self.knob) * self.warm;
            let left = delayed_dry + delayed_wet * (1.0 - blend) + stft_l * blend;
            let right = delayed_dry_r + delayed_wet_r * (1.0 - blend) + stft_r * blend;
            wet_l[i] = finite(left);
            wet_r[i] = finite(right);
            if !self.idle {
                self.warm = (self.warm + 1.0 / self.frame as f32).min(1.0);
            }
        }
        self.filled = (self.filled + n).min(self.frame);
        if peak > 1.0e-7 {
            self.tail = self.latency;
        } else {
            self.tail = self.tail.saturating_sub(n as u32);
        }
        closed_at.filter(|_| self.idle)
    }

    fn enter_bypass(&mut self) {
        self.idle = true;
        self.warm = 0.0;
        self.gains.fill(1.0);
        self.out_l.fill(0.0);
        self.out_r.fill(0.0);
    }

    fn smooth_knob(&mut self, amount: f32) {
        if amount == 0.0 && self.knob < 1.0e-4 {
            self.knob = 0.0;
            return;
        }
        self.knob += self.knob_coeff * (amount - self.knob);
        if (self.knob - amount).abs() < 1.0e-6 {
            self.knob = amount;
        }
    }

    fn process_hop(&mut self) {
        window_into(&self.wet_l, &self.window, self.pos, &mut self.time_l);
        window_into(&self.wet_r, &self.window, self.pos, &mut self.time_r);
        if self
            .forward
            .process_with_scratch(&mut self.time_l, &mut self.spec_l, &mut self.scratch)
            .is_err()
        {
            return;
        }
        if self
            .forward
            .process_with_scratch(&mut self.time_r, &mut self.spec_r, &mut self.scratch)
            .is_err()
        {
            return;
        }

        let bins = self.spec_l.len().min(self.spec_r.len()).min(self.gains.len());
        let mut peak = 0.0f32;
        for i in 0..bins {
            let mag = self.spec_l[i].norm().max(self.spec_r[i].norm());
            self.mags[i] = mag;
            if mag > peak {
                peak = mag;
            }
        }
        let threshold = peak * db_to_gain(threshold_db(self.knob));
        let floor = db_to_gain(floor_db(self.knob));
        for i in 0..bins {
            let target = if peak <= 1.0e-12 {
                1.0
            } else {
                gate_gain(self.mags[i], threshold, floor)
            };
            let coeff = if target >= self.gains[i] {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.gains[i] += coeff * (target - self.gains[i]);
            let gain = self.gains[i];
            self.spec_l[i].re *= gain;
            self.spec_l[i].im *= gain;
            self.spec_r[i].re *= gain;
            self.spec_r[i].im *= gain;
        }

        if self
            .inverse
            .process_with_scratch(&mut self.spec_l, &mut self.time_l, &mut self.scratch)
            .is_err()
        {
            return;
        }
        if self
            .inverse
            .process_with_scratch(&mut self.spec_r, &mut self.time_r, &mut self.scratch)
            .is_err()
        {
            return;
        }
        add_ring(&mut self.out_l, &self.time_l, self.pos);
        add_ring(&mut self.out_r, &self.time_r, self.pos);
    }
}

/// Nearest power of two to 40 ms. 44.1 kHz and 48 kHz both land on 2048.
pub fn frame_samples(sample_rate: f32) -> usize {
    let target = (sample_rate * 0.040).max(128.0);
    let power = target.log2().round().clamp(7.0, 16.0);
    2.0f32.powf(power) as usize
}

/// Threshold relative to the frame peak. Anchored at -60 dB, -30 dB, and -12 dB.
fn threshold_db(knob: f32) -> f32 {
    let k = knob.clamp(0.0, 1.0) as f64;
    let db = -104.142_857_142_857_14 + k * 170.714_285_714_285_72 + k * k * -78.571_428_571_428_57;
    db as f32
}

/// Closed-gate floor. About -40 dB toward the bottom of the knob, -80 dB at full.
fn floor_db(knob: f32) -> f32 {
    let k = knob.clamp(0.0, 1.0);
    -40.0 - 40.0 * k * k
}

fn wet_mix(knob: f32) -> f32 {
    let t = (knob / RAMP_KNOB).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn gate_gain(mag: f32, threshold: f32, floor: f32) -> f32 {
    if threshold <= 1.0e-12 {
        return 1.0;
    }
    let half = db_to_gain(KNEE_DB * 0.5);
    let lower = threshold / half;
    let upper = threshold * half;
    if mag >= upper {
        1.0
    } else if mag <= lower {
        floor
    } else {
        let pos = ((mag / lower).ln() / (upper / lower).ln()).clamp(0.0, 1.0);
        let s = pos * pos * (3.0 - 2.0 * pos);
        let floor_db = 20.0 * floor.max(1.0e-8).log10();
        db_to_gain(floor_db * (1.0 - s))
    }
}

fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn one_pole_coeff(elapsed: f32, time: f32) -> f32 {
    if time <= 1.0e-6 {
        1.0
    } else {
        (1.0 - (-elapsed / time).exp()).clamp(0.0, 1.0)
    }
}

fn finite(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

/// Sum of a periodic Hann window at this hop. Flat for a 75% overlap.
fn cola_sum(frame: usize, hop: usize) -> f32 {
    let hop = hop.max(1);
    let mut sum = 0.0f32;
    let mut i = 0;
    while i < frame {
        let phase = i as f32 / frame as f32;
        sum += 0.5 - 0.5 * (std::f32::consts::TAU * phase).cos();
        i += hop;
    }
    sum.max(1.0e-4)
}

fn window_into(ring: &[f32], window: &[f32], start: usize, dest: &mut [f32]) {
    let n = ring.len();
    let first = n - start;
    for i in 0..first {
        dest[i] = ring[start + i] * window[i];
    }
    for i in 0..start {
        dest[first + i] = ring[i] * window[first + i];
    }
}

fn add_ring(ring: &mut [f32], src: &[f32], start: usize) {
    let n = ring.len();
    let first = n - start;
    for i in 0..first {
        ring[start + i] += src[i];
    }
    for i in 0..start {
        ring[i] += src[first + i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_2048_at_common_rates_and_tracks_forty_ms() {
        assert_eq!(frame_samples(44_100.0), 2048);
        assert_eq!(frame_samples(48_000.0), 2048);
        let wide = frame_samples(96_000.0);
        assert_eq!(wide, 4096);
        let ms = wide as f32 / 96_000.0 * 1000.0;
        assert!(ms > 30.0 && ms < 50.0, "96 kHz frame should stay near 40 ms, {ms}");
    }

    #[test]
    fn threshold_curve_hits_the_anchor_points() {
        assert!((threshold_db(0.3) + 60.0).abs() < 0.05, "{}", threshold_db(0.3));
        assert!((threshold_db(0.6) + 30.0).abs() < 0.05, "{}", threshold_db(0.6));
        assert!((threshold_db(1.0) + 12.0).abs() < 0.05, "{}", threshold_db(1.0));
        assert!(threshold_db(0.0) < threshold_db(0.3));
        assert!(threshold_db(0.3) < threshold_db(0.6));
        assert!(threshold_db(0.6) < threshold_db(1.0));
    }

    #[test]
    fn floor_stays_open_at_the_bottom_and_closes_at_the_top() {
        assert!((floor_db(0.0) + 40.0).abs() < 0.01);
        assert!(floor_db(0.3) < -40.0 && floor_db(0.3) > -50.0);
        assert!((floor_db(1.0) + 80.0).abs() < 0.01);
    }

    #[test]
    fn knee_is_open_above_and_at_the_floor_below() {
        let floor = db_to_gain(-40.0);
        let threshold = db_to_gain(-30.0);
        let open = gate_gain(db_to_gain(-20.0), threshold, floor);
        let shut = gate_gain(db_to_gain(-50.0), threshold, floor);
        assert!((open - 1.0).abs() < 1.0e-4, "open {open}");
        assert!((shut - floor).abs() < 1.0e-4, "shut {shut}");
        let mid = gate_gain(threshold, threshold, floor);
        assert!(mid < 0.9 && mid > floor * 2.0, "knee center {mid}");
    }

    #[test]
    fn bypass_is_a_pure_delay_of_one_frame() {
        let mut gate = SpectralGate::new(44_100.0);
        let latency = gate.latency_samples() as usize;
        assert_eq!(latency, 2048);
        let mut wet_l = vec![0.0f32; latency * 3];
        let mut wet_r = vec![0.0f32; latency * 3];
        let dry_l = vec![0.0f32; latency * 3];
        let dry_r = vec![0.0f32; latency * 3];
        let mut state = 0x1234_5678u32;
        for (index, sample) in wet_l.iter_mut().enumerate() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *sample = (state >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0;
            wet_r[index] = *sample * 0.5;
        }
        let input_l = wet_l.clone();
        let input_r = wet_r.clone();
        gate.process_block(&mut wet_l, &mut wet_r, &dry_l, &dry_r, 0.0);
        for i in 0..latency {
            assert_eq!(wet_l[i], 0.0, "latency should lead with silence at {i}");
            assert_eq!(wet_r[i], 0.0);
        }
        for i in latency..input_l.len() {
            assert!(
                (wet_l[i] - input_l[i - latency]).abs() < 1.0e-6,
                "left sample {i} drifted"
            );
            assert!(
                (wet_r[i] - input_r[i - latency]).abs() < 1.0e-6,
                "right sample {i} drifted"
            );
        }
    }

    #[test]
    fn open_gate_reconstructs_a_sine() {
        let mut gate = SpectralGate::new(44_100.0);
        let latency = gate.latency_samples() as usize;
        let n = latency * 6;
        let mut wet_l = vec![0.0f32; n];
        let mut wet_r = vec![0.0f32; n];
        for i in 0..n {
            let t = i as f32 / 44_100.0;
            wet_l[i] = (std::f32::consts::TAU * 440.0 * t).sin() * 0.4;
            wet_r[i] = (std::f32::consts::TAU * 440.0 * t).sin() * 0.25;
        }
        let input = wet_l.clone();
        let dry = vec![0.0f32; n];
        gate.process_block(&mut wet_l, &mut wet_r, &dry, &dry, 0.05);
        let start = latency * 3;
        let mut err = 0.0f32;
        let mut energy = 0.0f32;
        for i in start..n {
            let diff = wet_l[i] - input[i - latency];
            err += diff * diff;
            energy += input[i - latency] * input[i - latency];
        }
        let ratio = err / energy.max(1.0e-12);
        assert!(
            ratio < 1.0e-3,
            "an almost-open gate should pass the tone, error ratio {ratio}"
        );
    }

    #[test]
    fn full_knob_keeps_the_loud_partial_and_strips_the_quiet_one() {
        let mut gate = SpectralGate::new(48_000.0);
        assert_eq!(gate.latency_samples(), 2048);
        let n = 48_000;
        let mut wet_l = vec![0.0f32; n];
        let mut wet_r = vec![0.0f32; n];
        for i in 0..n {
            let t = i as f32 / 48_000.0;
            let loud = (std::f32::consts::TAU * 440.0 * t).sin() * 0.5;
            let quiet = (std::f32::consts::TAU * 3_000.0 * t).sin() * 0.01;
            wet_l[i] = loud + quiet;
            wet_r[i] = loud * 0.2 + quiet;
        }
        let dry = vec![0.0f32; n];
        gate.process_block(&mut wet_l, &mut wet_r, &dry, &dry, 1.0);
        let start = 24_000;
        let body = &wet_l[start..];
        let loud = goertzel(body, 48_000.0, 440.0);
        let quiet = goertzel(body, 48_000.0, 3_000.0);
        assert!(loud > 0.05, "the loud partial should remain, {loud}");
        assert!(
            quiet < loud * 0.02,
            "the partial under the threshold should be stripped, quiet {quiet} loud {loud}"
        );
    }

    #[test]
    fn shared_decision_keeps_a_quiet_channel_of_a_loud_partial() {
        let mut gate = SpectralGate::new(44_100.0);
        let n = 44_100;
        let mut wet_l = vec![0.0f32; n];
        let mut wet_r = vec![0.0f32; n];
        for i in 0..n {
            let tone = (std::f32::consts::TAU * 440.0 * i as f32 / 44_100.0).sin();
            wet_l[i] = tone * 0.5;
            wet_r[i] = tone * 0.05;
        }
        let dry = vec![0.0f32; n];
        gate.process_block(&mut wet_l, &mut wet_r, &dry, &dry, 1.0);
        let right = goertzel(&wet_r[22_050..], 44_100.0, 440.0);
        assert!(
            right > 0.01,
            "the quiet side of a loud partial should stay, {right}"
        );
    }

    fn goertzel(samples: &[f32], sample_rate: f32, freq: f32) -> f32 {
        let omega = std::f32::consts::TAU * freq / sample_rate;
        let coeff = 2.0 * omega.cos();
        let mut s1 = 0.0f32;
        let mut s2 = 0.0f32;
        for sample in samples {
            let s0 = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / samples.len() as f32
    }
}
