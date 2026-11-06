//! Just enough WAV for handing audio to and from the workers: write mono 16-bit PCM, read PCM
//! (8/16/24/32-bit) or 32/64-bit float, mixed down to mono.

use std::path::Path;

use crate::BridgeError;

/// Mono samples and their rate.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mono {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// Write `samples` (−1..1) as a mono 16-bit PCM WAV.
pub fn write_mono16(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), BridgeError> {
    let data_len = u32::try_from(samples.len().saturating_mul(2)).map_err(|_| BridgeError::Io("audio too long for WAV".into()))?;
    let mut b = Vec::with_capacity(44 + samples.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b)?;
    Ok(())
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// Read a WAV file as mono.
pub fn read_mono(path: &Path) -> Result<Mono, BridgeError> {
    parse(&std::fs::read(path)?).ok_or_else(|| BridgeError::Io(format!("{}: not a WAV file this reader understands", path.display())))
}

fn parse(b: &[u8]) -> Option<Mono> {
    if b.get(0..4)? != b"RIFF" || b.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut i = 12;
    let (mut format, mut channels, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    let mut data: Option<&[u8]> = None;
    while i + 8 <= b.len() {
        let id = b.get(i..i + 4)?;
        let len = u32_at(b, i + 4)? as usize;
        let body = b.get(i + 8..(i + 8).saturating_add(len).min(b.len()))?;
        match id {
            b"fmt " => {
                format = u16_at(body, 0)?;
                channels = u16_at(body, 2)?;
                rate = u32_at(body, 4)?;
                bits = u16_at(body, 14)?;
                if format == 0xFFFE {
                    format = u16_at(body, 24)?; // WAVE_FORMAT_EXTENSIBLE: sub-format GUID's first two bytes
                }
            }
            b"data" => data = Some(body),
            _ => {}
        }
        i = i.saturating_add(8).saturating_add(len).saturating_add(len & 1);
    }
    let data = data?;
    let ch = usize::from(channels.max(1));
    let width = usize::from(bits / 8);
    if width == 0 || rate == 0 {
        return None;
    }
    let sample = |c: &[u8]| -> Option<f32> {
        Some(match (format, bits) {
            (1, 8) => (f32::from(*c.first()?) - 128.0) / 128.0,
            (1, 16) => f32::from(i16::from_le_bytes(c.try_into().ok()?)) / 32768.0,
            (1, 24) => (i32::from_le_bytes([0, *c.first()?, *c.get(1)?, *c.get(2)?]) >> 8) as f32 / 8_388_608.0,
            (1, 32) => i32::from_le_bytes(c.try_into().ok()?) as f32 / 2_147_483_648.0,
            (3, 32) => f32::from_le_bytes(c.try_into().ok()?),
            (3, 64) => f64::from_le_bytes(c.try_into().ok()?) as f32,
            _ => return None,
        })
    };
    let frame = width * ch;
    let mut samples = Vec::with_capacity(data.len() / frame.max(1));
    for f in data.chunks_exact(frame) {
        let mut sum = 0.0;
        for c in f.chunks_exact(width) {
            sum += sample(c)?;
        }
        samples.push(sum / ch as f32);
    }
    Some(Mono { samples, sample_rate: rate })
}

/// Linear resampling (enough for speech handed between models).
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || to == 0 || samples.is_empty() {
        return samples.to_vec();
    }
    let n = ((samples.len() as u64 * u64::from(to)) / u64::from(from)) as usize;
    let step = f64::from(from) / f64::from(to);
    (0..n)
        .map(|i| {
            let x = i as f64 * step;
            let j = x as usize;
            let t = (x - j as f64) as f32;
            let a = samples.get(j).copied().unwrap_or(0.0);
            let b = samples.get(j + 1).copied().unwrap_or(a);
            a + (b - a) * t
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_float() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.wav");
        let s: Vec<f32> = (0..1600).map(|i| (i as f32 / 50.0).sin() * 0.5).collect();
        write_mono16(&p, &s, 16_000).unwrap();
        let m = read_mono(&p).unwrap();
        assert_eq!((m.sample_rate, m.samples.len()), (16_000, 1600));
        assert!(m.samples.iter().zip(&s).all(|(a, b)| (a - b).abs() < 1e-3));

        // stereo float32, as torchaudio writes
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&24_000u32.to_le_bytes());
        b.extend_from_slice(&(24_000u32 * 8).to_le_bytes());
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [0.5f32, 0.25, -1.0, 0.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let m = parse(&b).unwrap();
        assert_eq!(m.sample_rate, 24_000);
        assert_eq!(m.samples, vec![0.375, -0.5]);
        assert!(parse(b"not a wav").is_none());
    }

    #[test]
    fn resamples() {
        assert_eq!(resample(&[0.0, 1.0, 0.0, 1.0], 24_000, 12_000), vec![0.0, 0.0]);
        assert_eq!(resample(&[0.0, 1.0], 8_000, 16_000).len(), 4);
    }
}
