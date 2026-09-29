//! `avcC` parsing and length-prefixed → Annex-B conversion.
//!
//! MP4 stores H.264 access units as a sequence of NAL units, each prefixed
//! by a big-endian length of 1, 2 or 4 bytes (the size is declared in the
//! `avcC` decoder configuration record). Decoders fed over a byte stream
//! (e.g. an ffmpeg child reading raw `h264`) want Annex-B instead: every NAL
//! prefixed by a `00 00 00 01` start code, with the SPS/PPS sent in-band.

/// The four-byte Annex-B start code prepended to every NAL unit.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// A parsed `AVCDecoderConfigurationRecord` (the payload of an `avcC` box).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    /// `AVCProfileIndication` (e.g. 66 baseline, 77 main, 100 high).
    pub profile: u8,
    /// `profile_compatibility` flags byte.
    pub compat: u8,
    /// `AVCLevelIndication` (level × 10, e.g. 31 for 3.1).
    pub level: u8,
    /// Size in bytes of each NAL length prefix in samples: 1, 2 or 4.
    pub nal_length_size: u8,
    /// Sequence parameter sets, without start codes or length prefixes.
    pub sps: Vec<Vec<u8>>,
    /// Picture parameter sets, without start codes or length prefixes.
    pub pps: Vec<Vec<u8>>,
}

impl AvcConfig {
    /// The parameter sets in Annex-B form: `START_CODE` + SPS for each SPS,
    /// then `START_CODE` + PPS for each PPS.
    pub fn parameter_sets(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in self.sps.iter().chain(&self.pps) {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        out
    }
}

/// Reads `count` parameter sets, each prefixed by a big-endian `u16` length.
fn read_sets(
    data: &[u8],
    pos: &mut usize,
    count: usize,
    what: &str,
) -> Result<Vec<Vec<u8>>, String> {
    let mut sets = Vec::with_capacity(count);
    for i in 0..count {
        let len_bytes = data
            .get(*pos..*pos + 2)
            .ok_or_else(|| format!("avcC truncated in {what} #{i} length"))?;
        let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
        let start = *pos + 2;
        let body = data
            .get(start..start + len)
            .ok_or_else(|| format!("avcC truncated in {what} #{i} ({len} bytes declared)"))?;
        sets.push(body.to_vec());
        *pos = start + len;
    }
    Ok(sets)
}

/// Parses an `avcC` box payload (`AVCDecoderConfigurationRecord`).
///
/// Trailing high-profile extension fields (chroma format, bit depths, SPS
/// extensions) are ignored.
///
/// # Errors
///
/// Returns a human-readable message when the record is truncated, its
/// `configurationVersion` is not 1, or it declares the invalid NAL length
/// size 3.
pub fn parse_avcc(data: &[u8]) -> Result<AvcConfig, String> {
    if data.len() < 6 {
        return Err(format!(
            "avcC too short ({} bytes, need at least 6)",
            data.len()
        ));
    }
    if data[0] != 1 {
        return Err(format!(
            "unsupported avcC configurationVersion {} (expected 1)",
            data[0]
        ));
    }
    let nal_length_size = (data[4] & 3) + 1;
    if nal_length_size == 3 {
        return Err("invalid avcC NAL length size 3 (must be 1, 2 or 4)".to_owned());
    }
    let mut pos = 6;
    let sps = read_sets(data, &mut pos, usize::from(data[5] & 0x1f), "SPS")?;
    let num_pps = *data
        .get(pos)
        .ok_or_else(|| "avcC truncated before PPS count".to_owned())?;
    pos += 1;
    let pps = read_sets(data, &mut pos, usize::from(num_pps), "PPS")?;
    Ok(AvcConfig {
        profile: data[1],
        compat: data[2],
        level: data[3],
        nal_length_size,
        sps,
        pps,
    })
}

/// Converts one length-prefixed sample to Annex-B, appending
/// `START_CODE` + NAL to `out` for every NAL unit in `sample`.
///
/// Zero-length NAL units are skipped. On error nothing is appended (`out`
/// is restored to its original length).
///
/// # Errors
///
/// Returns a message when `nal_length_size` is not 1, 2 or 4, when a length
/// prefix is cut off, or when a NAL's declared length overruns the sample.
pub fn to_annexb(sample: &[u8], nal_length_size: u8, out: &mut Vec<u8>) -> Result<(), String> {
    let n = usize::from(nal_length_size);
    if !matches!(n, 1 | 2 | 4) {
        return Err(format!(
            "invalid NAL length size {nal_length_size} (must be 1, 2 or 4)"
        ));
    }
    let original_len = out.len();
    let result = convert(sample, n, out);
    if result.is_err() {
        out.truncate(original_len);
    }
    result
}

/// The body of [`to_annexb`]; may leave `out` partially appended on error.
fn convert(sample: &[u8], n: usize, out: &mut Vec<u8>) -> Result<(), String> {
    let mut pos = 0usize;
    while pos < sample.len() {
        let prefix = sample.get(pos..pos + n).ok_or_else(|| {
            format!(
                "truncated NAL length prefix at byte {pos} ({} of {n} bytes present)",
                sample.len() - pos
            )
        })?;
        let len = prefix
            .iter()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        let start = pos + n;
        let remaining = sample.len() - start;
        let len = usize::try_from(len)
            .ok()
            .filter(|&l| l <= remaining)
            .ok_or_else(|| {
                format!("NAL at byte {pos} declares {len} bytes but only {remaining} remain")
            })?;
        if len > 0 {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(&sample[start..start + len]);
        }
        pos = start + len;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_avcc() -> Vec<u8> {
        vec![
            1, 100, 0, 31, 0xff, // version, profile, compat, level, 4-byte lengths
            0xe1, 0, 4, 0x67, 0x64, 0x00, 0x1f, // 1 SPS
            2, 0, 2, 0x68, 0xee, 0, 3, 0x68, 0xef, 0x01, // 2 PPS
        ]
    }

    #[test]
    fn parses_avcc() {
        let cfg = parse_avcc(&sample_avcc()).unwrap();
        assert_eq!(cfg.profile, 100);
        assert_eq!(cfg.compat, 0);
        assert_eq!(cfg.level, 31);
        assert_eq!(cfg.nal_length_size, 4);
        assert_eq!(cfg.sps, vec![vec![0x67, 0x64, 0x00, 0x1f]]);
        assert_eq!(cfg.pps, vec![vec![0x68, 0xee], vec![0x68, 0xef, 0x01]]);
        assert_eq!(
            cfg.parameter_sets(),
            vec![
                0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x68, 0xef,
                0x01
            ]
        );
    }

    #[test]
    fn avcc_trailing_extension_ignored() {
        let mut data = sample_avcc();
        data.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
        assert!(parse_avcc(&data).is_ok());
    }

    #[test]
    fn avcc_nal_length_sizes() {
        for (bits, expect) in [(0u8, Some(1u8)), (1, Some(2)), (2, None), (3, Some(4))] {
            let mut data = sample_avcc();
            data[4] = 0xfc | bits;
            match expect {
                Some(n) => assert_eq!(parse_avcc(&data).unwrap().nal_length_size, n),
                None => assert!(parse_avcc(&data).unwrap_err().contains("length size 3")),
            }
        }
    }

    #[test]
    fn avcc_bad_version() {
        let mut data = sample_avcc();
        data[0] = 0;
        assert!(
            parse_avcc(&data)
                .unwrap_err()
                .contains("configurationVersion")
        );
    }

    #[test]
    fn avcc_truncated_errors() {
        let data = sample_avcc();
        for n in 0..data.len() {
            assert!(parse_avcc(&data[..n]).is_err(), "prefix {n} parsed");
        }
    }

    #[test]
    fn annexb_all_length_sizes() {
        for size in [1u8, 2, 4] {
            let mut sample = Vec::new();
            for nal in [&[0x65u8, 1, 2][..], &[0x06], &[0x41, 9, 9, 9, 9]] {
                let len = nal.len() as u32;
                sample.extend_from_slice(&len.to_be_bytes()[4 - usize::from(size)..]);
                sample.extend_from_slice(nal);
            }
            let mut out = vec![0xaa];
            to_annexb(&sample, size, &mut out).unwrap();
            assert_eq!(
                out,
                vec![
                    0xaa, 0, 0, 0, 1, 0x65, 1, 2, 0, 0, 0, 1, 0x06, 0, 0, 0, 1, 0x41, 9, 9, 9, 9
                ]
            );
        }
    }

    #[test]
    fn annexb_empty_and_zero_length() {
        let mut out = Vec::new();
        to_annexb(&[], 4, &mut out).unwrap();
        assert!(out.is_empty());
        to_annexb(&[0, 0, 0, 0, 0, 0, 0, 1, 0x09], 4, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x09]);
    }

    #[test]
    fn annexb_truncated_errors_and_restores() {
        let sample = [0u8, 0, 0, 3, 0x65, 1, 2, 0, 0, 0, 2, 0x41, 7];
        let mut out = vec![1, 2, 3];
        to_annexb(&sample, 4, &mut out).unwrap();
        for n in 1..sample.len() {
            if n == 7 {
                continue; // a clean NAL boundary
            }
            let mut out = vec![1, 2, 3];
            assert!(to_annexb(&sample[..n], 4, &mut out).is_err(), "prefix {n}");
            assert_eq!(out, vec![1, 2, 3]);
        }
        let mut out = Vec::new();
        assert!(to_annexb(&[0xff, 0xff, 0xff, 0xff, 1], 4, &mut out).is_err());
        assert!(to_annexb(&[5, 1], 1, &mut out).is_err());
        assert!(to_annexb(&[0, 5, 1], 2, &mut out).is_err());
    }

    #[test]
    fn annexb_invalid_length_size() {
        let mut out = Vec::new();
        for size in [0u8, 3, 5, 8] {
            assert!(to_annexb(&[0, 0, 0, 1, 9], size, &mut out).is_err());
        }
    }
}
