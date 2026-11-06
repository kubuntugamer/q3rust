//! `snd_wav.c`, as far as the game's own sound files need it: RIFF/WAVE PCM,
//! decoded to the mixer's format.
//!
//! The engine resamples every sound to its single output format when the sound
//! is loaded (`ResampleSfx`), so a voice is already the right rate and channel
//! count by the time the mixer sees it. The stock weapon sounds are 22 kHz
//! mono, 16-bit PCM; the device here runs at 48 kHz stereo in `f32`.

/// The mixer's output rate: `dma.speed` on this client.
pub const OUTPUT_RATE: u32 = crate::audio::SAMPLE_RATE as u32;

/// Why a file could not be decoded, standing in for the C's refusal to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavError {
    /// Not a RIFF/WAVE file, or a chunk meant to be there is missing.
    NotWave,
    /// A chunk ran past the end of the file.
    Truncated,
    /// A format the game never ships, such as a compressed codec.
    UnsupportedFormat(u16),
    /// A bit depth other than 8 or 16.
    UnsupportedBits(u16),
    /// No channels, or no sample rate to resample from.
    BadFormat,
}

/// Decodes a RIFF/WAVE PCM file into interleaved stereo `f32` at
/// [`OUTPUT_RATE`], ready for the mixer to add into its buffer.
pub fn decode(bytes: &[u8]) -> Result<Vec<f32>, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }

    let mut format = None;
    let mut data = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = at + 8;
        if body + size > bytes.len() {
            return Err(WavError::Truncated);
        }
        match id {
            b"fmt " => {
                if size < 16 {
                    return Err(WavError::NotWave);
                }
                let chunk = &bytes[body..body + size];
                let kind = u16::from_le_bytes(chunk[0..2].try_into().unwrap());
                let channels = u16::from_le_bytes(chunk[2..4].try_into().unwrap());
                let rate = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes(chunk[14..16].try_into().unwrap());
                format = Some((kind, channels, rate, bits));
            }
            b"data" => data = Some(&bytes[body..body + size]),
            _ => {}
        }
        // Chunks are word aligned, so an odd size carries one pad byte.
        at = body + size + (size & 1);
    }

    let (kind, channels, rate, bits) = format.ok_or(WavError::NotWave)?;
    let data = data.ok_or(WavError::NotWave)?;
    if kind != 1 {
        return Err(WavError::UnsupportedFormat(kind));
    }
    if bits != 8 && bits != 16 {
        return Err(WavError::UnsupportedBits(bits));
    }
    if channels == 0 || rate == 0 {
        return Err(WavError::BadFormat);
    }

    // Samples per source frame, and the source as one plane per channel.
    let bytes_per = (bits / 8) as usize;
    let stride = bytes_per * channels as usize;
    let frames = data.len() / stride;
    let sample_at = |frame: usize, channel: usize| -> f32 {
        let at = frame * stride + channel * bytes_per;
        if bits == 16 {
            let value = i16::from_le_bytes([data[at], data[at + 1]]);
            value as f32 / 32768.0
        } else {
            (data[at] as f32 - 128.0) / 128.0
        }
    };

    // A mono file feeds both output channels; anything wider is cut to stereo.
    let sample = |frame: usize, channel: usize| -> f32 {
        if channel == 0 || channels == 1 {
            sample_at(frame, 0)
        } else {
            sample_at(frame, 1)
        }
    };

    // `ResampleSfx`: each output frame reads between two source frames, so a
    // sound at the device's rate passes through untouched.
    let out_frames = frames * OUTPUT_RATE as usize / rate as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    let step = rate as f64 / OUTPUT_RATE as f64;
    for frame in 0..out_frames {
        let source = frame as f64 * step;
        let first = source.floor() as usize;
        let second = (first + 1).min(frames.saturating_sub(1));
        let frac = (source - first as f64) as f32;
        for channel in 0..2 {
            let value = sample(first.min(frames.saturating_sub(1)), channel) * (1.0 - frac)
                + sample(second, channel) * frac;
            out.push(value);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-chunk RIFF/WAVE file: mono 16-bit at `rate`, one sample per frame.
    fn wave(rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        file.extend_from_slice(b"WAVEfmt ");
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes()); // PCM
        file.extend_from_slice(&1u16.to_le_bytes()); // mono
        file.extend_from_slice(&rate.to_le_bytes());
        file.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
        file.extend_from_slice(&2u16.to_le_bytes()); // block align
        file.extend_from_slice(&16u16.to_le_bytes()); // bits
        file.extend_from_slice(b"data");
        file.extend_from_slice(&(data.len() as u32).to_le_bytes());
        file.extend_from_slice(&data);
        file
    }

    #[test]
    fn a_stereo_pair_per_frame_comes_out_at_the_mixers_rate() {
        let samples = [16384i16, -16384, 8192];
        let decoded = decode(&wave(22_050, &samples)).unwrap();
        assert_eq!(decoded.len() % 2, 0);
        // 22050 -> 48000 is a little over double the frame count.
        assert_eq!(decoded.len() / 2, samples.len() * 48_000 / 22_050);
        assert!(decoded[0] > 0.4 && decoded[0] < 0.6, "{}", decoded[0]);
        assert!(decoded[1] > 0.4, "the mono sample fills both channels");
    }

    #[test]
    fn a_sound_already_at_the_device_rate_is_not_stretched() {
        let decoded = decode(&wave(OUTPUT_RATE, &[1000, 2000, 3000, 4000])).unwrap();
        assert_eq!(decoded.len(), 8);
    }

    #[test]
    fn what_is_not_a_wave_file_is_refused() {
        assert_eq!(decode(b"not a sound"), Err(WavError::NotWave));
        assert_eq!(decode(&[]), Err(WavError::NotWave));
        let mut truncated = wave(22_050, &[1, 2, 3]);
        truncated.truncate(30);
        assert_eq!(decode(&truncated), Err(WavError::Truncated));
    }
}
