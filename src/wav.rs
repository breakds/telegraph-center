//! Minimal RIFF/WAVE header parsing for PCM metadata.
//!
//! This is a pure, allocation-free reader over a byte slice. It is intended to
//! run over the leading bytes of an uploaded file: it reads chunk *headers*
//! (including the declared `data` chunk size) without requiring the full audio
//! payload to be present. v1 does not transcode and accepts flexible WAV
//! parameters, so this only extracts metadata; it does not enforce a specific
//! sample rate, channel count, or bit depth.

/// Audio metadata extracted from a WAV header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavInfo {
    /// Samples per second.
    pub sample_rate_hz: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Bits per sample.
    pub bits_per_sample: u16,
    /// Average bytes per second, as declared in the `fmt ` chunk.
    pub byte_rate: u32,
    /// Declared length of the `data` chunk in bytes, if it was seen.
    ///
    /// This is the size from the chunk header, which may exceed the bytes
    /// actually present in the slice that was parsed.
    pub data_len_bytes: Option<u32>,
}

impl WavInfo {
    /// Duration in milliseconds, computed from the declared data length and
    /// byte rate. Returns `None` if either is unavailable.
    pub fn duration_ms(&self) -> Option<i64> {
        match self.data_len_bytes {
            Some(len) if self.byte_rate > 0 => {
                Some((i64::from(len) * 1000) / i64::from(self.byte_rate))
            }
            _ => None,
        }
    }
}

/// A reason a byte slice could not be parsed as a WAV header.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WavError {
    /// The slice does not begin with a `RIFF` container marker.
    #[error("not a RIFF container")]
    NotRiff,
    /// The RIFF container is not a `WAVE` form.
    #[error("not a WAVE file")]
    NotWave,
    /// No usable `fmt ` chunk was found.
    #[error("missing or malformed fmt chunk")]
    MissingFmt,
    /// The header was structurally invalid.
    #[error("malformed WAV header")]
    Malformed,
}

const FMT_BODY_LEN: usize = 16;

/// Parse a WAV header from the leading bytes of a file.
pub fn parse_header(bytes: &[u8]) -> Result<WavInfo, WavError> {
    if bytes.len() < 12 {
        return Err(WavError::Malformed);
    }
    if &bytes[0..4] != b"RIFF" {
        return Err(WavError::NotRiff);
    }
    if &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }

    let mut offset = 12;
    let mut fmt: Option<(u32, u16, u16, u32)> = None;
    let mut data_len: Option<u32> = None;

    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let body = offset + 8;

        if id == b"fmt " {
            if body + FMT_BODY_LEN > bytes.len() {
                return Err(WavError::Malformed);
            }
            let channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
            let sample_rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            let byte_rate = u32::from_le_bytes(bytes[body + 8..body + 12].try_into().unwrap());
            let bits_per_sample = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            fmt = Some((sample_rate, channels, bits_per_sample, byte_rate));
        } else if id == b"data" {
            // The data chunk size comes from the header; the payload itself may
            // be truncated in the slice we were given, so stop scanning here.
            data_len = Some(size);
            break;
        }

        // Chunks are word-aligned: an odd size is padded with one byte.
        let advance = 8 + size as usize + (size as usize & 1);
        offset += advance;
    }

    let (sample_rate_hz, channels, bits_per_sample, byte_rate) = fmt.ok_or(WavError::MissingFmt)?;
    Ok(WavInfo {
        sample_rate_hz,
        channels,
        bits_per_sample,
        byte_rate,
        data_len_bytes: data_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal PCM WAV with the given parameters and `data_len` bytes
    /// of (zeroed) sample data.
    fn pcm_wav(sample_rate: u32, channels: u16, bits: u16, data_len: u32) -> Vec<u8> {
        let byte_rate = sample_rate * u32::from(channels) * (u32::from(bits) / 8);
        let block_align = channels * (bits / 8);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.extend(std::iter::repeat_n(0u8, data_len as usize));
        v
    }

    #[test]
    fn parses_minimal_pcm_wav() {
        let bytes = pcm_wav(16_000, 1, 16, 32_000);
        let info = parse_header(&bytes).unwrap();
        assert_eq!(info.sample_rate_hz, 16_000);
        assert_eq!(info.channels, 1);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.byte_rate, 32_000);
        assert_eq!(info.data_len_bytes, Some(32_000));
        // 32_000 bytes / 32_000 bytes-per-second = 1000 ms.
        assert_eq!(info.duration_ms(), Some(1000));
    }

    #[test]
    fn parses_header_without_full_data_payload() {
        // Truncate to just past the data chunk header: metadata still parses.
        let full = pcm_wav(44_100, 2, 16, 4096);
        let header_only = &full[..44];
        let info = parse_header(header_only).unwrap();
        assert_eq!(info.sample_rate_hz, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.data_len_bytes, Some(4096));
    }

    #[test]
    fn rejects_non_riff() {
        assert_eq!(
            parse_header(b"not a wav file at all").unwrap_err(),
            WavError::NotRiff
        );
    }

    #[test]
    fn rejects_non_wave_riff() {
        let mut bytes = pcm_wav(16_000, 1, 16, 0);
        bytes[8..12].copy_from_slice(b"AVI ");
        assert_eq!(parse_header(&bytes).unwrap_err(), WavError::NotWave);
    }

    #[test]
    fn rejects_too_short() {
        assert_eq!(parse_header(b"RIFF").unwrap_err(), WavError::Malformed);
    }

    #[test]
    fn rejects_missing_fmt() {
        // RIFF/WAVE with only a data chunk and no fmt chunk.
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&12u32.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"data");
        v.extend_from_slice(&0u32.to_le_bytes());
        // data chunk is reached first and parsing stops before any fmt.
        assert_eq!(parse_header(&v).unwrap_err(), WavError::MissingFmt);
    }
}
