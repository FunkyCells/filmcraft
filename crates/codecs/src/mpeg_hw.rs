//! H.264 from an MPEG transport stream (Annex B byte streams) with the OS hardware decoder as the
//! way out for what our own decoder does not do, such as field-coded (interlaced PAFF) pictures
//! from AVCHD camcorders.
//!
//! Our software decoder always goes first, so every stream it can decode keeps decoding exactly
//! as before. When it answers [`CodecError::Unsupported`], and Settings ▸ Playback ▸ Hardware
//! decoding allows it, the access units are rewritten to the length-prefixed form the registered
//! hardware decoders take (`avcC` parameter sets from the stream's own SPS / PPS) and handed to
//! [`make_video_decoder`], which tries the registered OS decoders before our own. A hardware
//! decoder that declines the stream leaves the same "unsupported" error as before.

use filmcraft_isobmff::{AvcConfig, SampleEntry};

use crate::video::{H264Decoder, VideoDecoder};
use crate::{CodecError, DecodedFrame, Result, make_video_decoder};

/// How many distinct parameter sets of each kind are remembered.
const MAX_PARAMETER_SETS: usize = 8;

/// The NAL units of an Annex B byte stream, without start codes or trailing zero bytes.
pub(crate) fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> = data.windows(3).enumerate().filter(|(_, w)| *w == [0, 0, 1]).map(|(i, _)| i + 3).collect();
    let mut out = Vec::new();
    for (k, &begin) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(data.len(), |&next| next.saturating_sub(3));
        let Some(nal) = data.get(begin..end) else { continue };
        let keep = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        if let Some(nal) = nal.get(..keep).filter(|n| !n.is_empty()) {
            out.push(nal);
        }
    }
    out
}

/// `nals` as one length-prefixed access unit; `None` when it would not fit 32-bit lengths.
fn length_prefixed(nals: &[&[u8]]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(nals.iter().map(|n| n.len() + 4).sum());
    for n in nals {
        out.extend_from_slice(&u32::try_from(n.len()).ok()?.to_be_bytes());
        out.extend_from_slice(n);
    }
    Some(out)
}

/// Remember `nal` in `sets` unless it is there already; the oldest goes when the list is full.
/// Whether it was new.
fn remember(sets: &mut Vec<Vec<u8>>, nal: &[u8]) -> bool {
    if sets.iter().any(|s| s == nal) {
        return false;
    }
    if sets.len() >= MAX_PARAMETER_SETS {
        sets.remove(0);
    }
    sets.push(nal.to_vec());
    true
}

/// Whether an SPS NAL unit (header byte included) declares field or MBAFF coding
/// (`frame_mbs_only_flag` 0), which our decoder does not do.
fn sps_is_interlaced(sps: &[u8]) -> bool {
    let parsed = sps.get(1..).and_then(|b| craft_video::h264::decode::params::Sps::parse(&filmcraft_bitstream::unescape_rbsp(b)).ok());
    parsed.is_some_and(|p| !p.frame_mbs_only)
}

/// The picture size of an SPS NAL unit (header byte included), cropped.
fn sps_size(sps: &[u8]) -> Option<(u16, u16)> {
    let parsed = craft_video::h264::decode::params::Sps::parse(&filmcraft_bitstream::unescape_rbsp(sps.get(1..)?)).ok()?;
    let (_, _, w, h) = parsed.crop_rect();
    Some((u16::try_from(w).ok()?, u16::try_from(h).ok()?))
}

/// Our decoder for an Annex B H.264 stream, with the hardware decoders behind it.
pub(crate) struct AnnexBH264 {
    sw: H264Decoder,
    /// Set once our decoder has refused the stream: the decoder that takes over.
    other: Option<Box<dyn VideoDecoder>>,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    /// An SPS of the stream declares field or MBAFF coding: our decoder is not tried.
    interlaced: bool,
}

/// An Annex B H.264 decoder (our own, or the OS one for what ours cannot decode).
pub(crate) fn h264() -> Box<dyn VideoDecoder> {
    Box::new(AnnexBH264 { sw: H264Decoder::annexb(), other: None, sps: Vec::new(), pps: Vec::new(), interlaced: false })
}

impl AnnexBH264 {
    fn note_parameter_sets(&mut self, nals: &[&[u8]]) {
        for nal in nals {
            match nal.first().map(|h| h & 0x1f) {
                Some(7) => {
                    if remember(&mut self.sps, nal) {
                        self.interlaced |= sps_is_interlaced(nal);
                    }
                }
                Some(8) => {
                    remember(&mut self.pps, nal);
                }
                _ => {}
            }
        }
    }

    /// The decoder to continue with, or `refused` when there is none to try.
    fn takeover(&self, refused: String) -> Result<Box<dyn VideoDecoder>> {
        if !crate::hw::hardware_decoding() {
            return Err(CodecError::Unsupported(refused));
        }
        let (w, h) = self.sps.iter().find_map(|s| sps_size(s)).ok_or_else(|| CodecError::Unsupported(refused.clone()))?;
        if self.pps.is_empty() {
            return Err(CodecError::Unsupported(refused));
        }
        let entry = SampleEntry::avc(AvcConfig::new(self.sps.clone(), self.pps.clone(), 4), w, h);
        // a hardware decoder that takes the stream counts a session; without one the decoder
        // made is our own, which cannot decode it
        let sessions = crate::hw::hw_stats().sessions;
        let decoder = make_video_decoder(&entry)?;
        if crate::hw::hw_stats().sessions == sessions {
            log::info!("no OS hardware decoder took the stream: {refused}");
            return Err(CodecError::Unsupported(format!("{refused}; no hardware decoder took the stream")));
        }
        Ok(decoder)
    }
}

impl VideoDecoder for AnnexBH264 {
    fn decode(&mut self, sample: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        let nals = annexb_nals(sample);
        self.note_parameter_sets(&nals);
        // field-coded streams go straight to the OS decoder; ours would stop at the first field
        // picture, and not necessarily with an "unsupported" error
        if self.other.is_none() && self.interlaced {
            self.other = Some(self.takeover("field-coded (interlaced) H.264".into())?);
        }
        if self.other.is_none() {
            match self.sw.decode(sample, pts) {
                Err(CodecError::Unsupported(why)) => self.other = Some(self.takeover(why)?),
                done => return done,
            }
        }
        let unit = length_prefixed(&nals).ok_or_else(|| CodecError::Decode("access unit too large".into()))?;
        match self.other.as_mut() {
            Some(d) => {
                let name = d.name().to_owned();
                d.decode(&unit, pts).map_err(|e| match e {
                    CodecError::Decode(m) => CodecError::Decode(format!("{name}: {m}")),
                    CodecError::Unsupported(m) => CodecError::Unsupported(format!("{name}: {m}")),
                    other => other,
                })
            }
            None => Err(CodecError::Unsupported("H.264".into())),
        }
    }

    fn flush(&mut self) -> Vec<DecodedFrame> {
        match self.other.as_mut() {
            Some(d) => d.flush(),
            None => self.sw.flush(),
        }
    }

    fn reset(&mut self) {
        self.sw.reset();
        if let Some(d) = self.other.as_mut() {
            d.reset();
        }
    }

    fn name(&self) -> &str {
        self.other.as_ref().map_or_else(|| self.sw.name(), |d| d.name())
    }

    fn is_random_access(&self, sample: &[u8]) -> Option<bool> {
        self.sw.is_random_access(sample)
    }

    fn is_disposable(&self, sample: &[u8]) -> bool {
        self.sw.is_disposable(sample)
    }

    fn set_draft(&mut self, on: bool) {
        self.sw.set_draft(on);
        if let Some(d) = self.other.as_mut() {
            d.set_draft(on);
        }
    }

    fn thread_limited(&self) -> bool {
        self.sw.thread_limited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_units_are_split_without_start_codes_or_trailing_zeros() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9, 0, 0];
        assert_eq!(annexb_nals(&data), vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 9, 9]]);
        for hostile in [&[][..], &[0], &[0, 0, 1], &[0, 0, 0, 0, 0, 0], &[0, 0, 1, 0, 0, 1, 0, 0, 1], &[1, 2, 3]] {
            let _ = annexb_nals(hostile);
        }
        assert!(annexb_nals(&[0, 0, 1]).is_empty());
    }

    #[test]
    fn access_units_become_length_prefixed() {
        let nals: Vec<&[u8]> = vec![&[0x67, 1], &[0x65, 2, 3]];
        assert_eq!(length_prefixed(&nals).unwrap(), vec![0, 0, 0, 2, 0x67, 1, 0, 0, 0, 3, 0x65, 2, 3]);
        assert_eq!(length_prefixed(&[]).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn remembered_parameter_sets_are_distinct_and_bounded() {
        let mut sets = Vec::new();
        remember(&mut sets, &[0x67, 1]);
        remember(&mut sets, &[0x67, 1]);
        assert_eq!(sets.len(), 1);
        for i in 0..40u8 {
            remember(&mut sets, &[0x67, i, 9]);
        }
        assert_eq!(sets.len(), MAX_PARAMETER_SETS);
        assert_eq!(sets.last(), Some(&vec![0x67, 39, 9]));
    }

    #[test]
    fn a_1080i_sps_is_interlaced_and_garbage_is_not() {
        let sps = [
            0x27, 0x64, 0x00, 0x28, 0xad, 0x00, 0x2d, 0xb0, 0x1e, 0x01, 0x11, 0xf7, 0x80, 0x88, 0x00, 0x00, 0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x01, 0x97, 0x26, 0x00,
            0x00, 0x80, 0x2c, 0x80, 0x03, 0x34, 0x54, 0x1e, 0x30, 0x64, 0xc0, 0x00, 0x10, 0x05, 0x90, 0x00, 0x66, 0x8a, 0x83, 0xc6, 0x05,
        ];
        assert!(sps_is_interlaced(&sps));
        assert_eq!(sps_size(&sps), Some((1920, 1080)));
        for junk in [&[][..], &[0x67], &[0x67, 0xff, 0xff], &[0x67, 0, 0, 0, 0, 0, 0]] {
            assert!(!sps_is_interlaced(junk));
        }
    }

    #[test]
    fn a_refused_stream_without_parameter_sets_keeps_the_original_error() {
        let d = AnnexBH264 { sw: H264Decoder::annexb(), other: None, sps: Vec::new(), pps: Vec::new(), interlaced: false };
        assert!(matches!(d.takeover("field pictures".into()), Err(CodecError::Unsupported(m)) if m == "field pictures"));
    }
}
