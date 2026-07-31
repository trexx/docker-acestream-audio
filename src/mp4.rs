//! Fragmented-MP4 box scanning for late joiners.
//!
//! A client that attaches mid-stream cannot just be handed live bytes: fMP4
//! needs the init segment (`ftyp` + `moov`) before any media, and must start on
//! a fragment boundary. This scanner is a pass-through — it never consumes
//! bytes, it only reports where the structure boundaries fall so the fan-out
//! layer can cache the init segment and align newcomers to the next `moof`.
//!
//! ADTS and MP3 need none of this: they are self-synchronising, so every byte
//! offset is a valid join point.

use bytes::Bytes;

/// ISO-BMFF box header: 4-byte big-endian size, then 4-byte ASCII type.
const SHORT_HEADER: usize = 8;
/// `size == 1` means a 64-bit `largesize` follows the type.
const LONG_HEADER: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Part of the init segment: everything preceding the first `moof`.
    Init(Bytes),
    /// Media bytes. `fragment_start` marks a run beginning at a `moof` header,
    /// which is where a late joiner may safely be attached.
    Media { data: Bytes, fragment_start: bool },
}

impl Piece {
    pub fn data(&self) -> &Bytes {
        match self {
            Piece::Init(d) => d,
            Piece::Media { data, .. } => data,
        }
    }

    pub fn fragment_start(&self) -> bool {
        matches!(
            self,
            Piece::Media {
                fragment_start: true,
                ..
            }
        )
    }
}

/// Media bytes accumulated within a single `push`, held only long enough to
/// coalesce a run into one piece. Never held across calls: that would trade
/// latency for tidiness, which is the wrong way round here.
#[derive(Debug, Default)]
struct Pending {
    parts: Vec<Bytes>,
    starts: bool,
}

impl Pending {
    fn take(&mut self) -> Option<Piece> {
        let starts = self.starts;
        self.starts = false;
        match self.parts.len() {
            0 => None,
            // The common case: one contiguous slice, so no copy at all.
            1 => Some(Piece::Media {
                data: self.parts.pop().unwrap(),
                fragment_start: starts,
            }),
            _ => {
                let len = self.parts.iter().map(|p| p.len()).sum();
                let mut merged = Vec::with_capacity(len);
                for p in self.parts.drain(..) {
                    merged.extend_from_slice(&p);
                }
                Some(Piece::Media {
                    data: Bytes::from(merged),
                    fragment_start: starts,
                })
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Scanner {
    /// Partial top-level box header carried across a chunk boundary.
    header: Vec<u8>,
    /// Bytes still to come in the current box body.
    body_left: u64,
    /// True until the first `moof` is seen.
    in_init: bool,
    init: Vec<u8>,
    /// Set when the box structure stops making sense. The scanner degrades to a
    /// pass-through rather than corrupting the stream; late joiners can no
    /// longer be served, which the caller checks via `failed`.
    failed: bool,
    started: bool,
    pending: Pending,
}

impl Scanner {
    pub fn new() -> Self {
        Scanner {
            in_init: true,
            ..Default::default()
        }
    }

    /// The cached init segment, once the first `moof` has been reached.
    pub fn init_segment(&self) -> Option<Bytes> {
        if self.in_init || self.init.is_empty() {
            None
        } else {
            Some(Bytes::copy_from_slice(&self.init))
        }
    }

    /// True if the byte stream did not parse as fragmented MP4. Late joiners
    /// cannot be served safely once this is set.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Split one chunk of ffmpeg output into structure-aligned pieces.
    ///
    /// The concatenation of the returned pieces always equals `chunk`, minus at
    /// most 15 bytes of box header held back until it is complete enough to
    /// identify. Nothing else is ever buffered across calls.
    pub fn push(&mut self, chunk: Bytes) -> Vec<Piece> {
        if self.failed {
            return vec![Piece::Media {
                data: chunk,
                fragment_start: false,
            }];
        }

        let mut out: Vec<Piece> = Vec::new();
        let mut i = 0usize;

        while i < chunk.len() {
            // Inside a box body: pass bytes straight through.
            if self.body_left > 0 {
                let take = self.body_left.min((chunk.len() - i) as u64) as usize;
                self.emit(&mut out, chunk.slice(i..i + take));
                self.body_left -= take as u64;
                i += take;
                continue;
            }

            // At a box boundary: accumulate a header.
            while self.header.len() < SHORT_HEADER && i < chunk.len() {
                self.header.push(chunk[i]);
                i += 1;
            }
            if self.header.len() < SHORT_HEADER {
                break; // need more bytes; header stays held back
            }

            let size32 = u32::from_be_bytes(self.header[0..4].try_into().unwrap());
            let (total, header_len) = if size32 == 1 {
                while self.header.len() < LONG_HEADER && i < chunk.len() {
                    self.header.push(chunk[i]);
                    i += 1;
                }
                if self.header.len() < LONG_HEADER {
                    break;
                }
                let large = u64::from_be_bytes(self.header[8..16].try_into().unwrap());
                (large, LONG_HEADER)
            } else if size32 == 0 {
                // "Extends to end of file" — never valid in a live fragmented
                // stream, and it would blind the scanner to every later box.
                return self.give_up(out, chunk, i);
            } else {
                (size32 as u64, SHORT_HEADER)
            };

            if total < header_len as u64 {
                return self.give_up(out, chunk, i);
            }

            let kind: [u8; 4] = self.header[4..8].try_into().unwrap();
            if !self.started {
                self.started = true;
                // A well-formed fMP4 stream opens with ftyp (or styp/moof if we
                // somehow attached mid-stream).
                if &kind != b"ftyp" && &kind != b"moof" && &kind != b"styp" {
                    return self.give_up(out, chunk, i);
                }
            }
            if &kind == b"moof" {
                self.in_init = false;
                // Close any run in progress so the fragment starts its own piece.
                if let Some(p) = self.pending.take() {
                    out.push(p);
                }
                self.pending.starts = true;
            }

            self.body_left = total - header_len as u64;
            let header = Bytes::copy_from_slice(&self.header);
            self.header.clear();
            self.emit(&mut out, header);
        }

        if let Some(p) = self.pending.take() {
            out.push(p);
        }
        out
    }

    fn emit(&mut self, out: &mut Vec<Piece>, data: Bytes) {
        if data.is_empty() {
            return;
        }
        if self.in_init {
            self.init.extend_from_slice(&data);
            out.push(Piece::Init(data));
            return;
        }
        self.pending.parts.push(data);
    }

    /// After a structural failure, hand back everything still unconsumed —
    /// including held-back header bytes — so the stream itself stays intact.
    fn give_up(&mut self, mut out: Vec<Piece>, chunk: Bytes, i: usize) -> Vec<Piece> {
        self.failed = true;
        self.in_init = false;
        // A partial init segment is worse than none: serving it to a late joiner
        // would produce a stream the client cannot decode.
        self.init.clear();

        if let Some(p) = self.pending.take() {
            out.push(p);
        }
        if !self.header.is_empty() {
            out.push(Piece::Media {
                data: Bytes::copy_from_slice(&self.header),
                fragment_start: false,
            });
            self.header.clear();
        }
        if i < chunk.len() {
            out.push(Piece::Media {
                data: chunk.slice(i..),
                fragment_start: false,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_of(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = (SHORT_HEADER + body.len()) as u32;
        let mut v = total.to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn large_box_of(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = (LONG_HEADER + body.len()) as u64;
        let mut v = 1u32.to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(&total.to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    /// A minimal but structurally valid fMP4 byte stream.
    /// Layout: ftyp(24) moov(48) | moof(28) mdat(108) | moof(28) mdat(88)
    fn stream() -> Vec<u8> {
        let mut v = box_of(b"ftyp", b"isom-brand-bytes");
        v.extend(box_of(b"moov", &vec![0xAA; 40]));
        v.extend(box_of(b"moof", &vec![0xBB; 20]));
        v.extend(box_of(b"mdat", &vec![0xCC; 100]));
        v.extend(box_of(b"moof", &vec![0xDD; 20]));
        v.extend(box_of(b"mdat", &vec![0xEE; 80]));
        v
    }

    const INIT_LEN: usize = 24 + 48;

    fn collect(pieces: &[Piece]) -> Vec<u8> {
        pieces.iter().flat_map(|p| p.data().to_vec()).collect()
    }

    /// Feed the stream in fixed-size slices to exercise header straddling.
    fn scan_in_chunks(data: &[u8], chunk: usize) -> (Scanner, Vec<Piece>) {
        let mut s = Scanner::new();
        let mut all = Vec::new();
        for c in data.chunks(chunk) {
            all.extend(s.push(Bytes::copy_from_slice(c)));
        }
        (s, all)
    }

    fn fragment_offsets(pieces: &[Piece]) -> Vec<usize> {
        let mut offsets = Vec::new();
        let mut at = 0usize;
        for p in pieces {
            if p.fragment_start() {
                offsets.push(at);
            }
            at += p.data().len();
        }
        offsets
    }

    #[test]
    fn is_lossless_at_every_chunk_size() {
        let data = stream();
        for size in 1..=data.len() {
            let (_, pieces) = scan_in_chunks(&data, size);
            assert_eq!(collect(&pieces), data, "chunk size {size} lost bytes");
        }
    }

    #[test]
    fn init_segment_is_everything_before_the_first_moof() {
        let data = stream();
        let want = &data[..INIT_LEN];
        for size in [1, 3, 7, 8, 9, 16, 64, 4096] {
            let (s, _) = scan_in_chunks(&data, size);
            assert_eq!(
                s.init_segment().as_deref(),
                Some(want),
                "chunk size {size}"
            );
        }
    }

    #[test]
    fn init_segment_is_unavailable_until_the_first_moof_arrives() {
        let mut s = Scanner::new();
        s.push(Bytes::from(box_of(b"ftyp", b"isom")));
        assert_eq!(s.init_segment(), None);
        s.push(Bytes::from(box_of(b"moov", &vec![0; 8])));
        assert_eq!(s.init_segment(), None, "moov alone must not complete init");
        s.push(Bytes::from(box_of(b"moof", &vec![0; 8])));
        assert!(s.init_segment().is_some());
    }

    #[test]
    fn fragment_starts_land_exactly_on_moof_headers() {
        let data = stream();
        for size in [1, 5, 8, 13, 64, 4096] {
            let (_, pieces) = scan_in_chunks(&data, size);
            assert_eq!(
                fragment_offsets(&pieces),
                vec![INIT_LEN, INIT_LEN + 28 + 108],
                "chunk {size}"
            );
        }
    }

    #[test]
    fn tolerates_free_boxes_inside_the_init_segment() {
        let mut data = box_of(b"ftyp", b"isom");
        data.extend(box_of(b"free", &vec![0; 16]));
        data.extend(box_of(b"moov", &vec![0xAA; 32]));
        data.extend(box_of(b"moof", &vec![0xBB; 8]));
        data.extend(box_of(b"mdat", &vec![0xCC; 8]));

        let (s, pieces) = scan_in_chunks(&data, 7);
        assert_eq!(collect(&pieces), data);
        assert_eq!(s.init_segment().map(|b| b.len()), Some(12 + 24 + 40));
        assert!(!s.failed());
    }

    #[test]
    fn handles_64_bit_largesize_boxes() {
        let mut data = box_of(b"ftyp", b"isom");
        data.extend(box_of(b"moov", &vec![0xAA; 16]));
        data.extend(box_of(b"moof", &vec![0xBB; 8]));
        // A large mdat is exactly where the 64-bit form shows up in practice.
        data.extend(large_box_of(b"mdat", &vec![0xCC; 200]));
        data.extend(box_of(b"moof", &vec![0xDD; 8]));

        for size in [1, 9, 15, 16, 17, 128] {
            let (s, pieces) = scan_in_chunks(&data, size);
            assert_eq!(collect(&pieces), data, "chunk {size}");
            assert!(!s.failed(), "chunk {size}");
            assert_eq!(
                fragment_offsets(&pieces).len(),
                2,
                "chunk {size}: largesize must not hide the second moof"
            );
        }
    }

    #[test]
    fn degrades_to_passthrough_on_garbage_without_losing_bytes() {
        // size == 0 means "to end of file" and is not valid in a live stream.
        let mut data = box_of(b"ftyp", b"isom");
        data.extend(0u32.to_be_bytes());
        data.extend(b"mdat");
        data.extend(vec![0xCC; 32]);

        for size in [1, 6, 13, 256] {
            let (s, pieces) = scan_in_chunks(&data, size);
            assert!(s.failed(), "chunk {size}");
            assert_eq!(collect(&pieces), data, "chunk {size}: must stay lossless");
            // A partial init segment would decode to nothing for a late joiner.
            assert_eq!(s.init_segment(), None, "chunk {size}");
        }
    }

    #[test]
    fn rejects_a_stream_that_does_not_start_with_ftyp() {
        let data = box_of(b"junk", &vec![0; 16]);
        let (s, pieces) = scan_in_chunks(&data, 4);
        assert!(s.failed());
        assert_eq!(collect(&pieces), data);
    }

    #[test]
    fn rejects_a_box_smaller_than_its_own_header() {
        let mut data = box_of(b"ftyp", b"isom");
        data.extend(3u32.to_be_bytes()); // size 3 < 8-byte header
        data.extend(b"moov");
        data.extend(vec![0; 8]);

        let (s, pieces) = scan_in_chunks(&data, 5);
        assert!(s.failed());
        assert_eq!(collect(&pieces), data);
    }

    #[test]
    fn media_runs_are_coalesced_within_a_chunk() {
        let data = stream();
        let mut s = Scanner::new();
        let pieces = s.push(Bytes::from(data));
        let media: Vec<_> = pieces.iter().filter(|p| !matches!(p, Piece::Init(_))).collect();
        // One piece per fragment, not one per box header and body.
        assert_eq!(media.len(), 2);
        assert!(media.iter().all(|p| p.fragment_start()));
        assert_eq!(media[0].data().len(), 28 + 108);
        assert_eq!(media[1].data().len(), 28 + 88);
    }

    #[test]
    fn nothing_is_buffered_across_pushes() {
        // Latency guard: a chunk that completes no box must still come straight
        // back out, minus only the held-back header.
        let mut s = Scanner::new();
        s.push(Bytes::from(box_of(b"ftyp", b"isom")));
        s.push(Bytes::from(box_of(b"moof", &vec![0; 64])));
        let out = s.push(Bytes::from(vec![0xCC; 32]));
        assert_eq!(collect(&out).len(), 32, "media must not be held back");
    }
}
