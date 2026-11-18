//! Diagnosis for a local transport-stream clip (macOS and Windows): does the OS hardware decoder
//! take its H.264, does it return pictures, and does `MpegSource` open the file and give a frame?
//! Each stage prints what happened, so a black Source monitor can be traced to one of them.
//!
//! ```text
//! FILMCRAFT_M2TS=/path/to/clip.m2ts cargo test -p filmcraft-platform --test m2ts_probe -- --nocapture
//! ```
//!
//! Does nothing when `FILMCRAFT_M2TS` is unset. The first stage needs `ffmpeg` on the PATH (it
//! cuts the clip's H.264 into a byte stream); without it that stage is skipped.

use std::sync::Arc;

use filmcraft_codecs::hw::NalStreamInfo;
use filmcraft_codecs::{MpegSource, VideoDecoder};
use filmcraft_isobmff::{AvcConfig, SampleEntry};
use filmcraft_media::{FrameRequest, MediaSource};
use filmcraft_time::Tick;

/// The NAL units of an Annex B byte stream.
fn nals(data: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> = data.windows(3).enumerate().filter(|(_, w)| *w == [0, 0, 1]).map(|(i, _)| i + 3).collect();
    let mut out = Vec::new();
    for (k, &begin) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(data.len(), |&next| next - 3);
        let nal = &data[begin..end];
        let keep = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        if keep > 0 {
            out.push(&nal[..keep]);
        }
    }
    out
}

/// Access units: a new one starts at each access unit delimiter (NAL type 9).
fn access_units<'a>(nals: &[&'a [u8]]) -> Vec<Vec<&'a [u8]>> {
    let mut out: Vec<Vec<&[u8]>> = Vec::new();
    for n in nals {
        if n[0] & 0x1f == 9 || out.is_empty() {
            out.push(Vec::new());
        }
        out.last_mut().unwrap().push(*n);
    }
    out
}

fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter().flat_map(|n| (n.len() as u32).to_be_bytes().into_iter().chain(n.iter().copied())).collect()
}

#[cfg(target_os = "macos")]
fn factory(entry: &SampleEntry) -> Option<filmcraft_codecs::Result<Box<dyn VideoDecoder>>> {
    filmcraft_platform::videotoolbox_factory(entry)
}
#[cfg(target_os = "windows")]
fn factory(entry: &SampleEntry) -> Option<filmcraft_codecs::Result<Box<dyn VideoDecoder>>> {
    filmcraft_platform::media_foundation_factory(entry)
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn factory(_: &SampleEntry) -> Option<filmcraft_codecs::Result<Box<dyn VideoDecoder>>> {
    None
}

/// Why the OS decoder declines the stream.
#[cfg(target_os = "macos")]
fn why_declined(_: &SampleEntry, info: &NalStreamInfo) -> String {
    match filmcraft_platform::videotoolbox::VtDecoder::new(info.clone()) {
        Ok(_) => "VideoToolbox created a session when asked directly".into(),
        Err(why) => why,
    }
}
#[cfg(target_os = "windows")]
fn why_declined(entry: &SampleEntry, _: &NalStreamInfo) -> String {
    use filmcraft_platform::media_foundation::{MfDecoder, stream_info};
    match stream_info(entry) {
        None => "Media Foundation has no interest in this stream".into(),
        Some(si) => match MfDecoder::new(si) {
            Ok(_) => "Media Foundation created a decoder when asked directly".into(),
            Err(why) => why,
        },
    }
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn why_declined(_: &SampleEntry, _: &NalStreamInfo) -> String {
    "no hardware backend on this OS".into()
}

/// Stage 1: the OS decoder on the clip's own H.264, without FilmCraft's media stack.
fn hardware_directly(path: &str) {
    let tmp = std::env::temp_dir().join("filmcraft-m2ts-probe.h264");
    let cut = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i", path, "-map", "0:v:0", "-c", "copy", "-frames:v", "40", "-f", "h264"])
        .arg(&tmp)
        .status();
    if !cut.is_ok_and(|s| s.success()) {
        eprintln!("stage 1 skipped: ffmpeg could not cut the H.264 out of the clip");
        return;
    }
    let data = std::fs::read(&tmp).unwrap_or_default();
    let all = nals(&data);
    let aus = access_units(&all);
    let pick = |t: u8| all.iter().filter(|n| n[0] & 0x1f == t).map(|n| n.to_vec()).take(1).collect::<Vec<_>>();
    let (sps, pps) = (pick(7), pick(8));
    if sps.is_empty() || pps.is_empty() {
        eprintln!("stage 1: no SPS / PPS found in the first pictures");
        return;
    }
    let entry = SampleEntry::avc(AvcConfig::new(sps, pps, 4), 1920, 1080);
    let info = match NalStreamInfo::from_entry(&entry) {
        Some(Ok(i)) => i,
        other => {
            eprintln!("stage 1: the stream parameters were not understood: {:?}", other.map(|r| r.map(|_| ())));
            return;
        }
    };
    eprintln!(
        "stage 1: profile {} chroma {} {}-bit interlaced {} coded {:?} crop {:?} reorder {}",
        info.profile_idc, info.chroma_format_idc, info.bit_depth_luma, info.interlaced, info.coded, info.crop, info.reorder
    );
    match factory(&entry) {
        None => eprintln!("stage 1: the OS decoder DECLINES the stream: {}", why_declined(&entry, &info)),
        Some(Err(e)) => eprintln!("stage 1: the OS decoder factory failed: {e}"),
        Some(Ok(mut d)) => {
            eprintln!("stage 1: the OS decoder accepted it as `{}`; feeding {} access units", d.name(), aus.len());
            let mut pictures = Vec::new();
            for (i, au) in aus.iter().enumerate() {
                match d.decode(&length_prefixed(au), i as i64) {
                    Ok(f) => pictures.extend(f),
                    Err(e) => {
                        eprintln!("stage 1: decode error at access unit {i}: {e}");
                        break;
                    }
                }
            }
            pictures.extend(d.flush());
            let sizes: std::collections::BTreeSet<(u32, u32)> = pictures.iter().map(|p| (p.frame.width, p.frame.height)).collect();
            eprintln!("stage 1: {} pictures out of {} access units, sizes {sizes:?}, decoder now `{}`", pictures.len(), aus.len(), d.name());
        }
    }
}

#[test]
fn local_m2ts_diagnosis() {
    let Ok(path) = std::env::var("FILMCRAFT_M2TS") else {
        eprintln!("set FILMCRAFT_M2TS=<clip> to run this diagnosis");
        return;
    };
    eprintln!("platform: {:?}, hardware decoding allowed: {}", filmcraft_platform::register(), filmcraft_codecs::hw::hardware_decoding());
    hardware_directly(&path);

    // Stage 2: the media stack, as the Source monitor uses it.
    let bytes: Arc<[u8]> = std::fs::read(&path).expect("read the clip").into();
    let src = match MpegSource::open("clip.m2ts", bytes) {
        Ok(s) => s,
        Err(e) => panic!("stage 2: the clip does not open: {e}"),
    };
    match &src.info().video {
        Some(v) => eprintln!("stage 2: opens: {}x{} {:.3} fps {} {}", v.width, v.height, v.frame_rate.as_f64(), v.codec, v.pixel_format),
        None => eprintln!("stage 2: opens, but without a video stream"),
    }
    for t in [Tick::ZERO, Tick(src.info().duration.0 / 2)] {
        match src.video_frame(FrameRequest::full(t)) {
            Ok(f) => eprintln!("stage 2: picture at {:.2}s: {}x{}", t.seconds(), f.width, f.height),
            Err(e) => eprintln!("stage 2: NO picture at {:.2}s: {e}", t.seconds()),
        }
    }
    eprintln!("hardware counters: {:?}", filmcraft_codecs::hw::hw_stats());
}
