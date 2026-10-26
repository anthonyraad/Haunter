//! The loaded sample: the copy embedded in the plugin state, and the copy resampled for playback.

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::audio_file::DecodedAudio;

/// Longer files are cut to this length so the project file stays a sensible size.
pub const MAX_SECONDS: f64 = 30.0;

/// The sample as loaded from disk, at its own sample rate. This is what goes into the plugin
/// state, so a project reopens with the sample even if the original file is gone.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedSample {
    pub name: String,
    pub sample_rate: f64,
    /// One or two channels, all the same length.
    pub channels: Vec<Vec<f32>>,
}

#[derive(Serialize, Deserialize)]
struct EmbeddedSampleRepr {
    name: String,
    sample_rate: f64,
    channels: usize,
    frames: usize,
    /// Interleaved little-endian f32, base64-encoded.
    pcm: String,
}

impl Serialize for EmbeddedSample {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let frames = self.frames();
        let mut bytes = Vec::with_capacity(frames * self.channels.len() * 4);
        for frame in 0..frames {
            for channel in &self.channels {
                bytes.extend_from_slice(&channel[frame].to_le_bytes());
            }
        }
        EmbeddedSampleRepr {
            name: self.name.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels.len(),
            frames,
            pcm: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EmbeddedSample {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let repr = EmbeddedSampleRepr::deserialize(deserializer)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(repr.pcm.as_bytes())
            .map_err(D::Error::custom)?;
        if repr.channels == 0 || repr.channels > 2 || bytes.len() != repr.frames * repr.channels * 4 {
            return Err(D::Error::custom("embedded sample has an inconsistent size"));
        }
        if !(repr.sample_rate.is_finite() && repr.sample_rate > 0.0) {
            return Err(D::Error::custom("embedded sample has an invalid sample rate"));
        }
        let mut channels = vec![Vec::with_capacity(repr.frames); repr.channels];
        for (index, chunk) in bytes.chunks_exact(4).enumerate() {
            channels[index % repr.channels].push(f32::from_le_bytes(chunk.try_into().unwrap()));
        }
        Ok(EmbeddedSample { name: repr.name, sample_rate: repr.sample_rate, channels })
    }
}

impl EmbeddedSample {
    /// Keeps at most two channels and [`MAX_SECONDS`] of audio.
    pub fn from_decoded(name: String, audio: DecodedAudio) -> Option<Self> {
        let max_frames = (MAX_SECONDS * audio.sample_rate) as usize;
        let mut channels: Vec<Vec<f32>> = audio.channels.into_iter().take(2).collect();
        for channel in &mut channels {
            channel.truncate(max_frames);
            for value in channel.iter_mut() {
                if !value.is_finite() {
                    *value = 0.0;
                }
            }
        }
        if channels.is_empty() || channels[0].is_empty() {
            return None;
        }
        Some(EmbeddedSample { name, sample_rate: audio.sample_rate, channels })
    }

    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / self.sample_rate
    }
}

/// Windowed-sinc sample rate conversion. Lowers the cutoff when downsampling so it doesn't alias.
pub fn resample(input: &[f32], from_rate: f64, to_rate: f64) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    if (from_rate - to_rate).abs() < 1e-9 {
        return input.to_vec();
    }
    const HALF_TAPS: f64 = 24.0;
    let ratio = to_rate / from_rate;
    let cutoff = ratio.min(1.0) * 0.97;
    let support = HALF_TAPS / cutoff;
    let out_len = ((input.len() as f64) * ratio).round().max(1.0) as usize;
    let step = 1.0 / ratio;
    let pi = std::f64::consts::PI;
    // The sinc and window phases advance by a fixed angle per input sample, so they are rotated
    // instead of evaluating sin/cos for every tap.
    let (sinc_step_sin, sinc_step_cos) = (pi * cutoff).sin_cos();
    let (window_step_sin, window_step_cos) = (pi / support).sin_cos();
    let mut output = Vec::with_capacity(out_len);
    for n in 0..out_len {
        let center = n as f64 * step;
        let first = (center - support).ceil().max(0.0) as usize;
        let last = ((center + support).floor().max(0.0) as usize).min(input.len() - 1);
        if first > last {
            output.push(0.0);
            continue;
        }
        let t0 = first as f64 - center;
        let (mut sinc_sin, mut sinc_cos) = (pi * cutoff * t0).sin_cos();
        // Blackman window over [-support, support], phase 2*pi*(0.5 + 0.5 * t / support).
        let (mut window_sin, mut window_cos) = (pi + pi * t0 / support).sin_cos();
        let mut acc = 0.0;
        for (offset, &value) in input[first..=last].iter().enumerate() {
            let x = (t0 + offset as f64) * cutoff;
            let sinc = if x.abs() < 1e-9 { 1.0 } else { sinc_sin / (pi * x) };
            let cos2 = 2.0 * window_cos * window_cos - 1.0;
            let window = 0.42 - 0.5 * window_cos + 0.08 * cos2;
            acc += value as f64 * cutoff * sinc * window;
            (sinc_sin, sinc_cos) = (
                sinc_sin * sinc_step_cos + sinc_cos * sinc_step_sin,
                sinc_cos * sinc_step_cos - sinc_sin * sinc_step_sin,
            );
            (window_sin, window_cos) = (
                window_sin * window_step_cos + window_cos * window_step_sin,
                window_cos * window_step_cos - window_sin * window_step_sin,
            );
        }
        output.push(acc as f32);
    }
    output
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The sample converted to the host's rate, ready for the audio thread.
#[derive(Debug)]
pub struct PreparedSample {
    /// Unique per preparation, so the engine knows when its analysis is out of date.
    pub id: u64,
    pub sample_rate: f64,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    /// Mono sum that keys the envelope follower.
    pub key: Vec<f32>,
}

impl PreparedSample {
    pub fn new(sample: &EmbeddedSample, host_rate: f64) -> Self {
        let left = resample(&sample.channels[0], sample.sample_rate, host_rate);
        let right = match sample.channels.get(1) {
            Some(channel) => resample(channel, sample.sample_rate, host_rate),
            None => left.clone(),
        };
        let key = left.iter().zip(&right).map(|(l, r)| 0.5 * (l + r)).collect();
        PreparedSample {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            sample_rate: host_rate,
            left,
            right,
            key,
        }
    }

    pub fn frames(&self) -> usize {
        self.left.len()
    }
}

/// Min/max pairs for drawing the waveform.
#[derive(Debug, Clone)]
pub struct Overview {
    pub name: String,
    pub seconds: f64,
    pub columns: Vec<(f32, f32)>,
}

impl Overview {
    pub fn new(sample: &EmbeddedSample, columns: usize) -> Self {
        let frames = sample.frames();
        let columns = (0..columns)
            .map(|column| {
                let from = column * frames / columns;
                let to = ((column + 1) * frames / columns).max(from + 1).min(frames);
                let mut low = 0.0f32;
                let mut high = 0.0f32;
                for frame in from..to {
                    let value = sample.channels.iter().map(|c| c[frame]).sum::<f32>() / sample.channels.len() as f32;
                    low = low.min(value);
                    high = high.max(value);
                }
                (low, high)
            })
            .collect();
        Overview { name: sample.name.clone(), seconds: sample.seconds(), columns }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, frames: usize) -> Vec<f32> {
        (0..frames).map(|n| (2.0 * std::f64::consts::PI * freq * n as f64 / rate).sin() as f32).collect()
    }

    #[test]
    fn resampling_keeps_pitch_and_level() {
        let input = sine(1000.0, 44_100.0, 44_100);
        let output = resample(&input, 44_100.0, 48_000.0);
        assert_eq!(output.len(), 48_000);
        let expected = sine(1000.0, 48_000.0, 48_000);
        // Ignore the edges, where the filter runs off the end of the input.
        let worst = output[200..47_800]
            .iter()
            .zip(&expected[200..47_800])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(worst < 2e-3, "worst error {worst}");
    }

    #[test]
    fn downsampling_removes_content_above_the_new_nyquist() {
        let input = sine(30_000.0, 96_000.0, 9_600);
        let output = resample(&input, 96_000.0, 44_100.0);
        let peak = output[200..output.len() - 200].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.01, "alias peak {peak}");
    }

    #[test]
    fn embedded_sample_round_trips_through_serde() {
        let sample = EmbeddedSample {
            name: "kick.wav".into(),
            sample_rate: 44_100.0,
            channels: vec![vec![0.0, 0.5, -1.0, 0.123_456_78], vec![1.0, -0.5, 0.25, -0.0]],
        };
        let json = serde_json::to_string(&sample).unwrap();
        let restored: EmbeddedSample = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, sample);
    }

    #[test]
    fn embedded_sample_rejects_mismatched_sizes() {
        let json = r#"{"name":"x","sample_rate":48000.0,"channels":2,"frames":3,"pcm":"AAAAAA=="}"#;
        assert!(serde_json::from_str::<EmbeddedSample>(json).is_err());
    }

    #[test]
    fn long_files_are_cut_and_extra_channels_dropped() {
        let audio = DecodedAudio { sample_rate: 1000.0, channels: vec![vec![0.1; 40_000]; 4] };
        let sample = EmbeddedSample::from_decoded("long".into(), audio).unwrap();
        assert_eq!(sample.channels.len(), 2);
        assert_eq!(sample.frames(), 30_000);
    }

    #[test]
    fn mono_samples_play_on_both_sides() {
        let sample = EmbeddedSample { name: "m".into(), sample_rate: 48_000.0, channels: vec![vec![0.5, -0.25]] };
        let prepared = PreparedSample::new(&sample, 48_000.0);
        assert_eq!(prepared.left, prepared.right);
        assert_eq!(prepared.key, vec![0.5, -0.25]);
    }
}
