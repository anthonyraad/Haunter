//! WAV, AIFF/AIFC, and MP3 decoding to floating point.

use std::fmt;
use std::path::Path;

use crate::sample::MAX_SECONDS;

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudio {
    pub sample_rate: f64,
    /// One vector per channel, all the same length.
    pub channels: Vec<Vec<f32>>,
}

impl DecodedAudio {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Io(String),
    NotAudio,
    Unsupported(String),
    Corrupt(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Io(message) => write!(f, "Could not read the file: {message}"),
            DecodeError::NotAudio => write!(f, "Not a WAV, AIFF, or MP3 file"),
            DecodeError::Unsupported(what) => write!(f, "Unsupported audio format: {what}"),
            DecodeError::Corrupt(what) => write!(f, "The file is damaged: {what}"),
        }
    }
}

pub fn decode_file(path: &Path) -> Result<DecodedAudio, DecodeError> {
    let bytes = std::fs::read(path).map_err(|err| DecodeError::Io(err.to_string()))?;
    let named_mp3 = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("mp3"));
    match decode(&bytes) {
        // Some MP3s start with padding or a tag other than ID3v2.
        Err(DecodeError::NotAudio) if named_mp3 => decode_mp3(&bytes),
        result => result,
    }
}

pub fn decode(bytes: &[u8]) -> Result<DecodedAudio, DecodeError> {
    if bytes.len() >= 12 {
        match (&bytes[0..4], &bytes[8..12]) {
            (b"RIFF", b"WAVE") => return decode_wav(bytes),
            (b"FORM", b"AIFF") => return decode_aiff(bytes, false),
            (b"FORM", b"AIFC") => return decode_aiff(bytes, true),
            _ => {}
        }
    }
    if looks_like_mp3(bytes) {
        return decode_mp3(bytes);
    }
    Err(DecodeError::NotAudio)
}

/// An ID3v2 tag, or the frame sync of an MPEG Layer III frame.
fn looks_like_mp3(bytes: &[u8]) -> bool {
    match bytes {
        [b'I', b'D', b'3', ..] => true,
        [0xFF, second, ..] => second & 0xE0 == 0xE0 && (second >> 1) & 0x03 == 0x01,
        _ => false,
    }
}

/// Decodes an MP3, without the encoder's delay and padding when the file says how long they are.
/// Stops after [`MAX_SECONDS`], since the rest would be cut anyway.
fn decode_mp3(bytes: &[u8]) -> Result<DecodedAudio, DecodeError> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes.to_vec())), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let options = FormatOptions { enable_gapless: true, ..Default::default() };
    let mut reader = symphonia::default::get_probe()
        .format(&hint, source, &options, &MetadataOptions::default())
        .map_err(|_| DecodeError::NotAudio)?
        .format;
    let track = reader.default_track().ok_or(DecodeError::Corrupt("no audio stream"))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|err| DecodeError::Unsupported(err.to_string()))?;

    let mut sample_rate = 0.0;
    let mut channels: Vec<Vec<f32>> = Vec::new();
    let mut buffer: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match reader.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired | Error::DecodeError(_)) => break,
            Err(_) => return Err(DecodeError::Corrupt("unreadable MP3 stream")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A damaged frame is skipped, the way players do.
            Err(Error::DecodeError(_)) => continue,
            Err(_) => return Err(DecodeError::Corrupt("undecodable MP3 frame")),
        };
        let spec = *decoded.spec();
        if channels.is_empty() {
            sample_rate = spec.rate as f64;
            channels = vec![Vec::new(); spec.channels.count()];
        }
        // A frame that switches between mono and stereo would leave the channels uneven.
        if spec.channels.count() != channels.len() || decoded.frames() == 0 {
            continue;
        }
        if buffer.as_ref().is_none_or(|buffer| buffer.capacity() < decoded.capacity()) {
            buffer = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
        }
        let buffer = buffer.as_mut().unwrap();
        let frames = decoded.frames();
        buffer.copy_planar_ref(decoded);
        for (channel, plane) in channels.iter_mut().zip(buffer.samples().chunks_exact(frames)) {
            channel.extend_from_slice(plane);
        }
        if channels[0].len() as f64 >= MAX_SECONDS * sample_rate {
            break;
        }
    }
    if channels.is_empty() {
        return Err(DecodeError::Corrupt("no MP3 frames"));
    }
    Ok(DecodedAudio { sample_rate, channels })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    /// 8-bit WAV data is unsigned, everything else is two's complement.
    UnsignedInt,
    SignedInt,
    Float,
}

#[derive(Clone, Copy)]
struct Layout {
    channels: usize,
    bits: usize,
    encoding: Encoding,
    big_endian: bool,
}

fn chunks(bytes: &[u8], big_endian: bool) -> impl Iterator<Item = ([u8; 4], &[u8])> + '_ {
    let mut offset = 12;
    std::iter::from_fn(move || {
        if offset + 8 > bytes.len() {
            return None;
        }
        let id: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
        let size_bytes: [u8; 4] = bytes[offset + 4..offset + 8].try_into().unwrap();
        let size = if big_endian {
            u32::from_be_bytes(size_bytes)
        } else {
            u32::from_le_bytes(size_bytes)
        } as usize;
        let start = offset + 8;
        // Tolerate a truncated final chunk, which some writers produce.
        let end = start.saturating_add(size).min(bytes.len());
        offset = start.saturating_add(size).saturating_add(size & 1);
        Some((id, &bytes[start..end]))
    })
}

fn le_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn be_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn decode_wav(bytes: &[u8]) -> Result<DecodedAudio, DecodeError> {
    let mut layout = None;
    let mut sample_rate = 0.0;
    let mut data = None;
    for (id, body) in chunks(bytes, false) {
        match &id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(DecodeError::Corrupt("short fmt chunk"));
                }
                let mut format = le_u16(body, 0);
                let channels = le_u16(body, 2) as usize;
                sample_rate = le_u32(body, 4) as f64;
                let bits = le_u16(body, 14) as usize;
                if format == 0xFFFE {
                    if body.len() < 26 {
                        return Err(DecodeError::Corrupt("short extensible fmt chunk"));
                    }
                    format = le_u16(body, 24);
                }
                let encoding = match (format, bits) {
                    (1, 8) => Encoding::UnsignedInt,
                    (1, 16 | 24 | 32) => Encoding::SignedInt,
                    (3, 32 | 64) => Encoding::Float,
                    (1 | 3, _) => return Err(DecodeError::Unsupported(format!("{bits}-bit WAV"))),
                    _ => return Err(DecodeError::Unsupported(format!("WAV format tag {format}"))),
                };
                layout = Some(Layout { channels, bits, encoding, big_endian: false });
            }
            b"data" => data = Some(body),
            _ => {}
        }
    }
    let layout = layout.ok_or(DecodeError::Corrupt("missing fmt chunk"))?;
    let data = data.ok_or(DecodeError::Corrupt("missing data chunk"))?;
    deinterleave(data, layout, sample_rate, None)
}

/// Converts an 80-bit IEEE 754 extended float, as used by AIFF's sample rate.
fn extended_to_f64(bytes: &[u8]) -> f64 {
    let sign = if bytes[0] & 0x80 != 0 { -1.0 } else { 1.0 };
    let exponent = (((bytes[0] & 0x7F) as i32) << 8) | bytes[1] as i32;
    let mantissa = u64::from_be_bytes(bytes[2..10].try_into().unwrap());
    if exponent == 0 && mantissa == 0 {
        return 0.0;
    }
    sign * mantissa as f64 * 2f64.powi(exponent - 16383 - 63)
}

fn decode_aiff(bytes: &[u8], compressed: bool) -> Result<DecodedAudio, DecodeError> {
    let mut layout = None;
    let mut sample_rate = 0.0;
    let mut frames = 0;
    let mut data = None;
    for (id, body) in chunks(bytes, true) {
        match &id {
            b"COMM" => {
                if body.len() < 18 {
                    return Err(DecodeError::Corrupt("short COMM chunk"));
                }
                let channels = be_u16(body, 0) as usize;
                frames = be_u32(body, 2) as usize;
                let bits = be_u16(body, 6) as usize;
                sample_rate = extended_to_f64(&body[8..18]);
                let compression: [u8; 4] = if compressed && body.len() >= 22 {
                    body[18..22].try_into().unwrap()
                } else {
                    *b"NONE"
                };
                let (encoding, big_endian, bits) = match &compression {
                    b"NONE" | b"twos" => (Encoding::SignedInt, true, bits),
                    b"sowt" => (Encoding::SignedInt, false, bits),
                    b"fl32" | b"FL32" => (Encoding::Float, true, 32),
                    b"fl64" | b"FL64" => (Encoding::Float, true, 64),
                    other => {
                        return Err(DecodeError::Unsupported(format!(
                            "AIFC compression '{}'",
                            String::from_utf8_lossy(other)
                        )))
                    }
                };
                if encoding == Encoding::SignedInt && !matches!(bits, 8 | 16 | 24 | 32) {
                    return Err(DecodeError::Unsupported(format!("{bits}-bit AIFF")));
                }
                layout = Some(Layout { channels, bits, encoding, big_endian });
            }
            b"SSND" => {
                if body.len() < 8 {
                    return Err(DecodeError::Corrupt("short SSND chunk"));
                }
                let offset = be_u32(body, 0) as usize;
                data = Some(body.get(8 + offset..).unwrap_or(&[]));
            }
            _ => {}
        }
    }
    let layout = layout.ok_or(DecodeError::Corrupt("missing COMM chunk"))?;
    let data = data.ok_or(DecodeError::Corrupt("missing SSND chunk"))?;
    deinterleave(data, layout, sample_rate, Some(frames))
}

fn deinterleave(
    data: &[u8],
    layout: Layout,
    sample_rate: f64,
    declared_frames: Option<usize>,
) -> Result<DecodedAudio, DecodeError> {
    if layout.channels == 0 {
        return Err(DecodeError::Corrupt("zero channels"));
    }
    if !(sample_rate.is_finite() && sample_rate >= 1000.0 && sample_rate <= 1_000_000.0) {
        return Err(DecodeError::Corrupt("implausible sample rate"));
    }
    let width = layout.bits / 8;
    let frame_bytes = width * layout.channels;
    let mut frames = data.len() / frame_bytes;
    if let Some(declared) = declared_frames {
        frames = frames.min(declared);
    }
    let mut channels = vec![Vec::with_capacity(frames); layout.channels];
    for frame in 0..frames {
        for (channel, out) in channels.iter_mut().enumerate() {
            let at = frame * frame_bytes + channel * width;
            out.push(read_sample(&data[at..at + width], layout));
        }
    }
    Ok(DecodedAudio { sample_rate, channels })
}

fn read_sample(raw: &[u8], layout: Layout) -> f32 {
    let mut buf = [0u8; 8];
    let width = raw.len();
    // Normalise to big-endian, left-aligned in the 8-byte buffer.
    for i in 0..width {
        buf[i] = if layout.big_endian { raw[i] } else { raw[width - 1 - i] };
    }
    match layout.encoding {
        Encoding::UnsignedInt => (buf[0] as f32 - 128.0) / 128.0,
        Encoding::SignedInt => {
            let value = i64::from_be_bytes(buf) >> (64 - layout.bits);
            value as f32 / (1i64 << (layout.bits - 1)) as f32
        }
        Encoding::Float if width == 4 => f32::from_be_bytes(buf[0..4].try_into().unwrap()),
        Encoding::Float => f64::from_be_bytes(buf) as f32,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn wav_bytes(format: u16, bits: u16, channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&format.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        let block = channels * bits / 8;
        out.extend_from_slice(&(rate * block as u32).to_le_bytes());
        out.extend_from_slice(&block.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    pub fn wav_i16(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        wav_bytes(1, 16, channels, rate, &data)
    }

    fn f64_to_extended(value: f64) -> [u8; 10] {
        let mut out = [0u8; 10];
        if value == 0.0 {
            return out;
        }
        let exponent = value.log2().floor() as i32;
        let mantissa = (value / 2f64.powi(exponent - 63)) as u64;
        let biased = (exponent + 16383) as u16;
        out[0..2].copy_from_slice(&biased.to_be_bytes());
        out[2..10].copy_from_slice(&mantissa.to_be_bytes());
        out
    }

    fn aiff_bytes(compression: Option<&[u8; 4]>, bits: u16, channels: u16, rate: f64, frames: u32, data: &[u8]) -> Vec<u8> {
        let mut comm = Vec::new();
        comm.extend_from_slice(&channels.to_be_bytes());
        comm.extend_from_slice(&frames.to_be_bytes());
        comm.extend_from_slice(&bits.to_be_bytes());
        comm.extend_from_slice(&f64_to_extended(rate));
        if let Some(kind) = compression {
            comm.extend_from_slice(kind);
            comm.extend_from_slice(&[0, 0]);
        }
        let mut body = Vec::new();
        body.extend_from_slice(if compression.is_some() { b"AIFC" } else { b"AIFF" });
        body.extend_from_slice(b"COMM");
        body.extend_from_slice(&(comm.len() as u32).to_be_bytes());
        body.extend_from_slice(&comm);
        body.extend_from_slice(b"SSND");
        body.extend_from_slice(&(8 + data.len() as u32).to_be_bytes());
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(data);
        if data.len() % 2 == 1 {
            body.push(0);
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn decodes_16_bit_stereo_wav() {
        let bytes = wav_i16(2, 44_100, &[16384, -16384, 32767, -32768]);
        let audio = decode(&bytes).unwrap();
        assert_eq!(audio.sample_rate, 44_100.0);
        assert_eq!(audio.channels.len(), 2);
        assert_eq!(audio.channels[0], vec![0.5, 32767.0 / 32768.0]);
        assert_eq!(audio.channels[1], vec![-0.5, -1.0]);
    }

    #[test]
    fn decodes_24_bit_and_float_and_8_bit_wav() {
        let data = [0x00, 0x00, 0x40, 0x00, 0x00, 0xC0]; // 0.5, -0.5
        let audio = decode(&wav_bytes(1, 24, 1, 48_000, &data)).unwrap();
        assert_eq!(audio.channels[0], vec![0.5, -0.5]);

        let data: Vec<u8> = [0.25f32, -0.75].iter().flat_map(|s| s.to_le_bytes()).collect();
        let audio = decode(&wav_bytes(3, 32, 1, 48_000, &data)).unwrap();
        assert_eq!(audio.channels[0], vec![0.25, -0.75]);

        let audio = decode(&wav_bytes(1, 8, 1, 22_050, &[128, 192, 0])).unwrap();
        assert_eq!(audio.channels[0], vec![0.0, 0.5, -1.0]);
    }

    #[test]
    fn decodes_aiff_and_aifc_variants() {
        let data: Vec<u8> = [16384i16, -16384].iter().flat_map(|s| s.to_be_bytes()).collect();
        let audio = decode(&aiff_bytes(None, 16, 1, 44_100.0, 2, &data)).unwrap();
        assert_eq!(audio.sample_rate, 44_100.0);
        assert_eq!(audio.channels[0], vec![0.5, -0.5]);

        let data: Vec<u8> = [16384i16, -16384].iter().flat_map(|s| s.to_le_bytes()).collect();
        let audio = decode(&aiff_bytes(Some(b"sowt"), 16, 2, 96_000.0, 1, &data)).unwrap();
        assert_eq!(audio.sample_rate, 96_000.0);
        assert_eq!(audio.channels, vec![vec![0.5], vec![-0.5]]);

        let data: Vec<u8> = [0.125f32].iter().flat_map(|s| s.to_be_bytes()).collect();
        let audio = decode(&aiff_bytes(Some(b"fl32"), 32, 1, 48_000.0, 1, &data)).unwrap();
        assert_eq!(audio.channels[0], vec![0.125]);

        let audio = decode(&aiff_bytes(None, 24, 1, 22_050.0, 1, &[0x40, 0, 0])).unwrap();
        assert_eq!(audio.channels[0], vec![0.5]);
    }

    /// 0.25 s of a 1 kHz sine at 44.1 kHz, 0.5 on the left and 0.25 on the right, with 5 ms
    /// fades, encoded by LAME at 128 kbps with its gapless Info tag.
    const TONE_MP3: &[u8] = include_bytes!("../tests/fixtures/tone-1khz.mp3");

    #[test]
    fn decodes_mp3_without_the_encoder_delay() {
        let audio = decode(TONE_MP3).unwrap();
        assert_eq!(audio.sample_rate, 44_100.0);
        assert_eq!(audio.channels.len(), 2);
        assert_eq!(audio.frames(), 11_025, "LAME's delay and padding are trimmed");

        let steady = 1_000..10_000;
        for (channel, amplitude) in audio.channels.iter().zip([0.5f32, 0.25]) {
            let peak = channel[steady.clone()].iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!((peak - amplitude).abs() < 0.03, "peak {peak}, expected {amplitude}");
            let crossings = channel[steady.clone()].windows(2).filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0)).count();
            let hertz = crossings as f64 / 2.0 / (steady.len() as f64 / 44_100.0);
            assert!((hertz - 1_000.0).abs() < 10.0, "{hertz} Hz");
        }
        // Without the trim the tone would start about 1100 samples late.
        assert!(audio.channels[0][300..400].iter().any(|v| v.abs() > 0.2), "the tone starts on time");
    }

    #[test]
    fn mp3s_are_recognised_by_their_contents() {
        let folder = crate::test_support::TempDir::new();
        std::fs::create_dir_all(folder.path()).unwrap();
        let renamed = folder.path().join("tone.wav");
        std::fs::write(&renamed, TONE_MP3).unwrap();
        assert_eq!(decode_file(&renamed).unwrap().frames(), 11_025);

        let mut padded = vec![0u8; 64];
        padded.extend_from_slice(TONE_MP3);
        assert_eq!(decode(&padded), Err(DecodeError::NotAudio));
        let named = folder.path().join("padded.MP3");
        std::fs::write(&named, &padded).unwrap();
        assert_eq!(decode_file(&named).unwrap().channels.len(), 2, "an .mp3 is tried as MP3 regardless");
    }

    #[test]
    fn rejects_other_files() {
        assert_eq!(decode(b"PK\x03\x04 not a wave file at all"), Err(DecodeError::NotAudio));
        assert!(decode(b"ID3 not an mp3 either, just the letters").is_err());
        assert!(decode(&TONE_MP3[..TONE_MP3.len() / 2]).is_ok_and(|audio| audio.frames() < 11_025), "a cut-off MP3 plays what is there");
        assert!(matches!(decode(&wav_bytes(2, 16, 1, 44_100, &[0, 0])), Err(DecodeError::Unsupported(_))));
    }
}
