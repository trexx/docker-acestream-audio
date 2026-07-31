//! Output formats and the copy-first decision table.
//!
//! ffmpeg reads raw MPEG-TS on stdin (fed by the shared engine puller) rather
//! than opening the engine URL itself, so there is exactly one engine pull per
//! content id no matter how many formats are being served from it.

use std::fmt;

/// What happens to a track on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackMode {
    /// Stream-copied untouched: no re-encode, no quality loss, near-zero CPU.
    Copy,
    Transcode,
    /// Track is dropped entirely (video on the audio-only endpoints).
    Dropped,
}

impl fmt::Display for TrackMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TrackMode::Copy => "copy",
            TrackMode::Transcode => "transcode",
            TrackMode::Dropped => "drop",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OutputFormat {
    Adts,
    Mp3,
    Fmp4,
}

impl OutputFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Adts => "adts",
            OutputFormat::Mp3 => "mp3",
            OutputFormat::Fmp4 => "fmp4",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            OutputFormat::Adts => "audio/aac",
            OutputFormat::Mp3 => "audio/mpeg",
            OutputFormat::Fmp4 => "video/mp4",
        }
    }

    /// Only fMP4 has an init segment that late joiners need replayed; ADTS and
    /// MP3 are self-synchronising, so any byte offset is a valid join point.
    pub fn needs_init_segment(self) -> bool {
        matches!(self, OutputFormat::Fmp4)
    }

    /// The `fmt=` values accepted on /audio.
    pub fn parse_audio(s: &str) -> Option<Self> {
        match s {
            "adts" => Some(OutputFormat::Adts),
            "mp3" => Some(OutputFormat::Mp3),
            _ => None,
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The chosen encoding for one (format, source codec) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub args: Vec<String>,
    pub video: TrackMode,
    pub audio: TrackMode,
}

/// Fragment duration override for fMP4, in milliseconds.
///
/// Unset means keyframe-only fragmentation: a `moof` cannot be written until
/// its fragment is complete, so fragment duration is a hard latency floor equal
/// to the source GOP. Setting this cuts fragments mid-GOP, which lowers latency
/// but lands late joiners on a non-keyframe boundary (artifacts until the next
/// IDR).
pub type FragDurationMs = Option<u32>;

fn s(v: &str) -> String {
    v.to_owned()
}

/// How much input ffmpeg may examine before it must have identified the
/// streams, as `(analyzeduration_us, probesize_bytes)`.
///
/// These are *not* free caps that `find_stream_info` exits early from — startup
/// tracks them almost one-for-one, so the value is a direct latency cost.
///
/// Video needs more than audio. Copying H.264 into MP4 requires SPS/PPS
/// extradata to build the `avcC` box; too small a window gives "non-existing
/// PPS 0 referenced" then "dimensions not set", and the muxer refuses to write
/// a header at all. Audio parameters come from any frame header.
///
/// Measured against 23MB of real 720p broadcast TS (1.92s GOP), attaching at 12
/// different offsets, with and without the [`REPLAY_BYTES`] window in front:
///
/// ```text
///   analyzeduration    cold      with replay
///   0.5s               0/5       0/5
///   1s                 1/5       0/5
///   2s                 5/5       5/5      <- floor, and 12/12 on a wider sweep
///   3s                 12/12     12/12
/// ```
///
/// The floor is ~2s of *content* regardless of where the encoder attaches, so
/// replay does not move it — its job is removing the intermittent total
/// failures, not shrinking this window. `probesize` was not the binding
/// constraint at any value from 1MB up.
///
/// This window does **not** need to exceed the source GOP, which is the obvious
/// but wrong intuition: a 1080p stream with a 4.0s GOP starts reliably on this
/// 3s window (6/6 cold starts against a real engine). [`REPLAY_BYTES`] is why —
/// ffmpeg is handed a keyframe in its opening bytes, so it never has to wait
/// one out. Shrinking the replay window would put that back in play.
///
/// 3s is therefore the ~2s content floor plus margin, not a GOP multiple.
/// Silently failing to start is far worse than a second of one-off latency, and
/// fan-out means only the *first* listener of a content id pays it at all.
fn probe_window(fmt: OutputFormat) -> (&'static str, &'static str) {
    match fmt {
        OutputFormat::Fmp4 => ("3000000", "10000000"),
        _ => ("1000000", "500000"),
    }
}

/// Input-side options. These must precede `-i`: `-fflags` and `-flags` are
/// demuxer/decoder options and are silently inert if placed after it.
fn input_args(fmt: OutputFormat) -> Vec<String> {
    let (analyzeduration, probesize) = probe_window(fmt);
    [
        "-hide_banner",
        "-loglevel",
        "error",
        // Position-sensitive: also set on the output side below.
        "-avioflags",
        "direct",
        // discardcorrupt matters for a P2P source, where corrupt and partial
        // frames are routine rather than exceptional.
        //
        // Deliberately *without* `nobuffer`, despite its billing as a
        // low-latency flag. Measured, it slows stream analysis and so delays
        // first output: ~2s worse on video at a 2s window, ~4s at 5s, ~0.6s on
        // audio. Output-side latency is handled by -flush_packets and
        // -avioflags direct, which cost nothing.
        "-fflags",
        "discardcorrupt",
        "-flags",
        "low_delay",
        "-analyzeduration",
        analyzeduration,
        "-probesize",
        probesize,
        "-max_delay",
        "0",
        // The input is always raw TS from the engine puller; naming the demuxer
        // skips format detection entirely.
        "-f",
        "mpegts",
        "-i",
        "pipe:0",
    ]
    .iter()
    .map(|v| s(v))
    .collect()
}

/// Output-side options common to every format. `-flush_packets 1` is the single
/// biggest steady-state latency win: without it ffmpeg's output AVIO buffers
/// ~32KB before writing to the pipe, which at 128 kbps is ~2 seconds.
fn flush_args() -> Vec<String> {
    ["-avioflags", "direct", "-flush_packets", "1"]
        .iter()
        .map(|v| s(v))
        .collect()
}

fn aac_encode() -> Vec<String> {
    ["-c:a", "aac", "-b:a", "128k", "-ac", "2"]
        .iter()
        .map(|v| s(v))
        .collect()
}

/// Build the full ffmpeg argv for one stream.
///
/// `audio_codec` is the source's first audio stream codec name as reported by
/// ffprobe (e.g. "aac", "ac3"); `None` means no audio stream was found.
pub fn plan(fmt: OutputFormat, audio_codec: Option<&str>, frag_ms: FragDurationMs) -> Plan {
    let mut args = input_args(fmt);
    let video;
    let audio;

    match fmt {
        OutputFormat::Adts | OutputFormat::Mp3 => {
            video = TrackMode::Dropped;
            args.extend(["-vn", "-sn", "-dn"].iter().map(|v| s(v)));

            let want = if fmt == OutputFormat::Adts { "aac" } else { "mp3" };
            if audio_codec == Some(want) {
                audio = TrackMode::Copy;
                args.extend(["-c:a", "copy"].iter().map(|v| s(v)));
            } else {
                audio = TrackMode::Transcode;
                if fmt == OutputFormat::Adts {
                    args.extend(aac_encode());
                } else {
                    args.extend(
                        ["-c:a", "libmp3lame", "-b:a", "128k", "-ac", "2"]
                            .iter()
                            .map(|v| s(v)),
                    );
                }
            }

            args.extend(flush_args());
            args.extend(["-f", fmt.as_str(), "pipe:1"].iter().map(|v| s(v)));
        }

        OutputFormat::Fmp4 => {
            // Video is always copied: that is where the CPU win is, and it is
            // what "zero copy" means here (no re-encode, not untouched bytes).
            video = TrackMode::Copy;
            args.extend(["-c:v", "copy"].iter().map(|v| s(v)));

            // AC-3, E-AC-3 and MP2 are legal in MP4 but browsers will not decode
            // them, so a blanket `-c copy` yields video with silence. Only AAC
            // survives the copy path.
            if audio_codec == Some("aac") {
                audio = TrackMode::Copy;
                args.extend(["-c:a", "copy"].iter().map(|v| s(v)));
                // MPEG-TS carries AAC in ADTS framing; MP4 wants raw AAC with
                // an AudioSpecificConfig in the sample entry. The muxer does
                // not convert on copy — without this it aborts the stream with
                // "Malformed AAC bitstream detected". Not needed on the /audio
                // routes, where ADTS is the native output framing, nor when
                // transcoding, since the encoder emits raw AAC.
                args.extend(["-bsf:a", "aac_adtstoasc"].iter().map(|v| s(v)));
            } else if audio_codec.is_some() {
                audio = TrackMode::Transcode;
                args.extend(aac_encode());
            } else {
                audio = TrackMode::Dropped;
                args.push(s("-an"));
            }

            // Broadcast TS carries timestamp discontinuities that MP4 rejects.
            args.extend(["-avoid_negative_ts", "make_zero"].iter().map(|v| s(v)));
            args.extend(flush_args());

            let mut movflags =
                s("+frag_keyframe+empty_moov+default_base_moof+omit_tfhd_offset");
            if let Some(ms) = frag_ms {
                // frag_duration is in microseconds.
                args.extend(["-frag_duration", &(ms as u64 * 1000).to_string()].iter().map(|v| s(v)));
                movflags = s("+empty_moov+default_base_moof+omit_tfhd_offset");
            }
            args.extend(["-movflags", &movflags].iter().map(|v| s(v)));
            args.extend(["-f", "mp4", "pipe:1"].iter().map(|v| s(v)));
        }
    }

    Plan { args, video, audio }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(fmt: OutputFormat, codec: Option<&str>) -> Vec<String> {
        plan(fmt, codec, None).args
    }

    /// Assert `needle` appears as a consecutive run in `hay`.
    fn contains_seq(hay: &[String], needle: &[&str]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn adts_copies_aac() {
        let p = plan(OutputFormat::Adts, Some("aac"), None);
        assert_eq!(p.audio, TrackMode::Copy);
        assert_eq!(p.video, TrackMode::Dropped);
        assert!(contains_seq(&p.args, &["-c:a", "copy"]));
        assert!(contains_seq(&p.args, &["-f", "adts", "pipe:1"]));
    }

    #[test]
    fn adts_transcodes_everything_else() {
        for codec in ["ac3", "eac3", "mp2", "dts", "mp3"] {
            let p = plan(OutputFormat::Adts, Some(codec), None);
            assert_eq!(p.audio, TrackMode::Transcode, "codec {codec}");
            assert!(contains_seq(&p.args, &["-c:a", "aac"]), "codec {codec}");
        }
    }

    #[test]
    fn mp3_copies_mp3_only() {
        let p = plan(OutputFormat::Mp3, Some("mp3"), None);
        assert_eq!(p.audio, TrackMode::Copy);
        assert!(contains_seq(&p.args, &["-c:a", "copy"]));

        let p = plan(OutputFormat::Mp3, Some("aac"), None);
        assert_eq!(p.audio, TrackMode::Transcode);
        assert!(contains_seq(&p.args, &["-c:a", "libmp3lame"]));
    }

    #[test]
    fn fmp4_always_copies_video() {
        for codec in [Some("aac"), Some("ac3"), Some("mp2"), None] {
            let p = plan(OutputFormat::Fmp4, codec, None);
            assert_eq!(p.video, TrackMode::Copy, "codec {codec:?}");
            assert!(contains_seq(&p.args, &["-c:v", "copy"]), "codec {codec:?}");
        }
    }

    #[test]
    fn fmp4_transcodes_browser_hostile_audio() {
        // AC-3/E-AC-3/MP2 are legal in MP4 but browsers play them as silence.
        for codec in ["ac3", "eac3", "mp2"] {
            let p = plan(OutputFormat::Fmp4, Some(codec), None);
            assert_eq!(p.audio, TrackMode::Transcode, "codec {codec}");
            assert!(contains_seq(&p.args, &["-c:a", "aac"]), "codec {codec}");
        }
        let p = plan(OutputFormat::Fmp4, Some("aac"), None);
        assert_eq!(p.audio, TrackMode::Copy);
    }

    #[test]
    fn fmp4_converts_adts_framing_when_copying_aac() {
        // Only on the copy path into MP4: the encoder already emits raw AAC,
        // and ADTS output wants ADTS framing.
        let p = plan(OutputFormat::Fmp4, Some("aac"), None);
        assert!(contains_seq(&p.args, &["-bsf:a", "aac_adtstoasc"]));

        let p = plan(OutputFormat::Fmp4, Some("ac3"), None);
        assert!(!p.args.iter().any(|a| a == "-bsf:a"));
        for fmt in [OutputFormat::Adts, OutputFormat::Mp3] {
            let p = plan(fmt, Some("aac"), None);
            assert!(!p.args.iter().any(|a| a == "-bsf:a"), "{fmt}");
        }
    }

    #[test]
    fn fmp4_without_audio_drops_the_track() {
        let p = plan(OutputFormat::Fmp4, None, None);
        assert_eq!(p.audio, TrackMode::Dropped);
        assert!(p.args.iter().any(|a| a == "-an"));
    }

    #[test]
    fn fmp4_movflags_are_mse_compatible() {
        let p = plan(OutputFormat::Fmp4, Some("aac"), None);
        let i = p.args.iter().position(|a| a == "-movflags").unwrap();
        let flags = &p.args[i + 1];
        for want in [
            "frag_keyframe",
            "empty_moov",
            "default_base_moof",
            "omit_tfhd_offset",
        ] {
            assert!(flags.contains(want), "missing {want} in {flags}");
        }
    }

    /// Guards against inventing plausible-but-wrong option names. ffmpeg
    /// describes this flag as "default-base-is-moof" in prose while naming the
    /// option `default_base_moof`, and gets it wrong loudly but late — the
    /// muxer refuses to write a header at all.
    #[test]
    fn movflags_are_accepted_by_the_installed_ffmpeg() {
        let out = match std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-h", "muxer=mp4"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return, // no ffmpeg here; nothing to check against
        };
        let help = String::from_utf8_lossy(&out.stdout);
        // Option lines lead with the option name.
        let accepted: Vec<&str> = help
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .collect();

        for frag in [None, Some(200)] {
            let p = plan(OutputFormat::Fmp4, Some("aac"), frag);
            let i = p.args.iter().position(|a| a == "-movflags").unwrap();
            for flag in p.args[i + 1].split('+').filter(|f| !f.is_empty()) {
                assert!(
                    accepted.contains(&flag),
                    "ffmpeg does not accept movflag {flag:?}"
                );
            }
        }
    }

    #[test]
    fn frag_duration_replaces_keyframe_fragmentation() {
        let p = plan(OutputFormat::Fmp4, Some("aac"), Some(200));
        assert!(contains_seq(&p.args, &["-frag_duration", "200000"]));
        let i = p.args.iter().position(|a| a == "-movflags").unwrap();
        // frag_keyframe would pin fragments back to the GOP and defeat the knob.
        assert!(!p.args[i + 1].contains("frag_keyframe"));
    }

    #[test]
    fn demuxer_options_precede_the_input() {
        // -fflags/-flags after -i are silently inert; guard against a reorder.
        for fmt in [OutputFormat::Adts, OutputFormat::Mp3, OutputFormat::Fmp4] {
            let args = args_of(fmt, Some("ac3"));
            let i = args.iter().position(|a| a == "-i").unwrap();
            let fflags = args.iter().position(|a| a == "-fflags").unwrap();
            let flags = args.iter().position(|a| a == "-flags").unwrap();
            assert!(fflags < i, "{fmt}");
            assert!(flags < i, "{fmt}");
        }
    }

    #[test]
    fn output_is_flushed_per_packet() {
        for fmt in [OutputFormat::Adts, OutputFormat::Mp3, OutputFormat::Fmp4] {
            let args = args_of(fmt, Some("aac"));
            assert!(contains_seq(&args, &["-flush_packets", "1"]), "{fmt}");
            // -avioflags direct is meaningful on both sides of -i.
            assert_eq!(args.iter().filter(|a| *a == "-avioflags").count(), 2, "{fmt}");
        }
    }

    #[test]
    fn input_is_always_the_shared_ts_pipe() {
        for fmt in [OutputFormat::Adts, OutputFormat::Mp3, OutputFormat::Fmp4] {
            let args = args_of(fmt, Some("aac"));
            assert!(contains_seq(&args, &["-f", "mpegts", "-i", "pipe:0"]), "{fmt}");
        }
    }

    #[test]
    fn audio_formats_drop_non_audio_tracks() {
        for fmt in [OutputFormat::Adts, OutputFormat::Mp3] {
            let args = args_of(fmt, Some("aac"));
            for flag in ["-vn", "-sn", "-dn"] {
                assert!(args.iter().any(|a| a == flag), "{fmt} missing {flag}");
            }
        }
    }

    #[test]
    fn parse_audio_rejects_video_format() {
        assert_eq!(OutputFormat::parse_audio("adts"), Some(OutputFormat::Adts));
        assert_eq!(OutputFormat::parse_audio("mp3"), Some(OutputFormat::Mp3));
        assert_eq!(OutputFormat::parse_audio("fmp4"), None);
        assert_eq!(OutputFormat::parse_audio(""), None);
    }
}
