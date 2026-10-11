//! Parser for the proprietary 4-channel DVR ".avc" container.
//!
//! Layout (reverse-engineered, little-endian):
//! - file header starting with "VA", two Windows SYSTEMTIME values at 0x08
//!   (file creation / trigger time) and 0x18 (wall-clock time of the first
//!   frame), then `CHn.Title=NAME\n` lines;
//! - frame records, each starting with `ff 3d 01 e0` (video) or `ff bd 01 e0`
//!   (audio). See [`Recording::parse`] for the record layout.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, bail};

const VIDEO_TAG: u8 = 0x3d;
const AUDIO_TAG: u8 = 0xbd;
/// Offset of the payload from the start of a video / audio record.
const VIDEO_HDR: usize = 46;
const AUDIO_HDR: usize = 62;

/// A Windows SYSTEMTIME, as stored in the file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DvrTime {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

impl DvrTime {
    fn read(d: &[u8], off: usize) -> Option<Self> {
        let f = |i: usize| u16_at(d, off + 2 * i);
        let t = DvrTime {
            year: f(0)?,
            month: f(1)?,
            // f(2) is the day of week, not needed.
            day: f(3)?,
            hour: f(4)?,
            minute: f(5)?,
            second: f(6)?,
            millis: f(7)?,
        };
        // Reject obviously bogus values rather than printing garbage.
        let valid = (1..=12).contains(&t.month)
            && (1..=31).contains(&t.day)
            && t.hour < 24
            && t.minute < 60
            && t.second < 61
            && t.millis < 1000;
        valid.then_some(t)
    }

    /// This time plus `ms` milliseconds, with day/month/year carry.
    pub fn add_millis(self, ms: u64) -> Self {
        let mut t = self;
        let total = u64::from(t.millis)
            + 1000 * (u64::from(t.second) + 60 * (u64::from(t.minute) + 60 * u64::from(t.hour)))
            + ms;
        t.millis = (total % 1000) as u16;
        t.second = (total / 1000 % 60) as u16;
        t.minute = (total / 60_000 % 60) as u16;
        t.hour = (total / 3_600_000 % 24) as u16;
        for _ in 0..total / 86_400_000 {
            t.day += 1;
            if t.day > days_in_month(t.year, t.month) {
                t.day = 1;
                t.month += 1;
                if t.month > 12 {
                    t.month = 1;
                    t.year = t.year.saturating_add(1);
                }
            }
        }
        t
    }
}

fn days_in_month(y: u16, m: u16) -> u16 {
    match m {
        2 if (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl fmt::Display for DvrTime {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millis
        )
    }
}

/// A demuxed recording. Frames are stored as ranges into `data`, not copies.
#[derive(Debug, Default)]
pub struct Recording {
    pub data: Vec<u8>,
    /// Channel number -> camera title.
    pub titles: BTreeMap<u16, String>,
    /// SYSTEMTIME at 0x08: probably file creation / trigger time.
    pub created: Option<DvrTime>,
    /// SYSTEMTIME at 0x18: wall-clock time of the first record (matches the
    /// first entry of the `.idx` sidecar). There is no end time in the header.
    pub first_frame: Option<DvrTime>,
    /// Channel number (1..4) -> `(timestamp ms, payload range)` of each H.264 frame.
    /// Timestamps are unwrapped across the u32 rollover, see [`Unwrap`].
    pub video: BTreeMap<u16, Vec<(u64, Range<usize>)>>,
    /// Channel number -> `(width, height)` of its first frame.
    pub sizes: BTreeMap<u16, (u16, u16)>,
    /// `(timestamp ms, payload range)` of each G.711 mu-law audio block.
    pub audio: Vec<(u64, Range<usize>)>,
}

/// Frame rate bounds; anything outside comes from corrupt timestamps.
const MIN_FPS: f64 = 0.1;
const MAX_FPS: f64 = 120.0;

/// Extends the DVR's u32 millisecond clock (it wraps every ~49.7 days of
/// uptime) to a monotonic-ish u64, by following the signed delta between
/// consecutive records in file order.
#[derive(Default)]
struct Unwrap {
    last: Option<(u32, i64)>,
}

impl Unwrap {
    fn next(&mut self, ts: u32) -> u64 {
        let ext = match self.last {
            None => i64::from(ts),
            Some((raw, ext)) => ext + i64::from(ts.wrapping_sub(raw) as i32),
        };
        self.last = Some((ts, ext));
        // Only a corrupt record before the first one can go negative.
        ext.max(0) as u64
    }
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

fn find(d: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    d.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Position of the next record candidate: the next `01 e0` at or after `from`, minus 2.
fn next_candidate(d: &[u8], from: usize) -> Option<usize> {
    let mut from = from;
    loop {
        let n = find(d, &[0x01, 0xe0], from)?;
        if n >= 2 {
            return Some(n - 2);
        }
        from = n + 1;
    }
}

impl Recording {
    pub fn open(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(data).with_context(|| format!("parsing {}", path.display()))
    }

    /// Parse a whole file. Record layout, relative to the marker:
    /// `+4 u32 seq, +8 u32 size, +12 u16 width, +14 u16 height, +16 u32 ts (ms),
    /// +20 u16 channel`; video payload at +46, audio payload at +62.
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        if !data.starts_with(b"VA") {
            bail!("not a DVR .avc file (missing \"VA\" header)");
        }
        let mut rec = Recording {
            created: DvrTime::read(&data, 0x08),
            first_frame: DvrTime::read(&data, 0x18),
            ..Default::default()
        };

        let mut clock = Unwrap::default();
        let mut pos = next_candidate(&data, 0);
        while let Some(o) = pos {
            let d = &data;
            let is_marker =
                d.get(o) == Some(&0xff) && matches!(d.get(o + 1), Some(&VIDEO_TAG) | Some(&AUDIO_TAG));
            if !is_marker {
                // Resync. Searching from o+3 always moves forward past this candidate.
                pos = next_candidate(d, o + 3);
                continue;
            }
            let hdr = if d[o + 1] == VIDEO_TAG { VIDEO_HDR } else { AUDIO_HDR };
            // Truncated final record: stop cleanly.
            let (Some(size), Some(ts), Some(ch)) = (u32_at(d, o + 8), u32_at(d, o + 16), u16_at(d, o + 20)) else {
                break;
            };
            // `size as usize` can't overflow on 64-bit; checked_add keeps 32-bit honest.
            let end = (o + hdr).checked_add(size as usize);
            let Some(end) = end.filter(|&e| e <= d.len()) else {
                // Corrupt size or truncated final record: resync on the next
                // marker rather than dropping everything after it.
                pos = next_candidate(d, o + 3);
                continue;
            };
            let payload = o + hdr..end;
            let ts = clock.next(ts);
            if d[o + 1] == VIDEO_TAG {
                let (w, h) = (u16_at(d, o + 12).unwrap_or(0), u16_at(d, o + 14).unwrap_or(0));
                rec.sizes.entry(ch).or_insert((w, h));
                rec.video.entry(ch).or_default().push((ts, payload.clone()));
            } else {
                rec.audio.push((ts, payload.clone()));
            }
            pos = Some(payload.end);
        }

        // Titles live in the header, before the first record.
        let header_end = rec
            .video
            .values()
            .flatten()
            .map(|(_, r)| r.start)
            .chain(rec.audio.iter().map(|(_, r)| r.start))
            .min()
            .unwrap_or(data.len().min(0x400));
        rec.titles = parse_titles(&data[..header_end]);
        rec.data = data;
        Ok(rec)
    }

    pub fn frame(&self, r: &Range<usize>) -> &[u8] {
        &self.data[r.clone()]
    }

    /// Earliest video timestamp (ms).
    pub fn t0(&self) -> u64 {
        self.video.values().filter_map(|f| f.first()).map(|f| f.0).min().unwrap_or(0)
    }

    /// Recording span in seconds, from the first to the last video frame.
    pub fn duration(&self) -> f64 {
        let last = self.video.values().filter_map(|f| f.last()).map(|f| f.0).max().unwrap_or(0);
        last.saturating_sub(self.t0()) as f64 / 1000.0
    }

    /// Wall-clock time of the last record, estimated as the header's
    /// first-frame time plus the span of the record timestamps. (The DVR's
    /// millisecond clock drifts a little against its wall clock.)
    pub fn last_frame(&self) -> Option<DvrTime> {
        let ts = self.video.values().flatten().chain(&self.audio).map(|f| f.0);
        let (lo, hi) = ts.fold((u64::MAX, 0), |(lo, hi), t| (lo.min(t), hi.max(t)));
        self.first_frame.filter(|_| hi >= lo).map(|t| t.add_millis(hi - lo))
    }

    /// Delay (s) between the recording start and the first frame of `ch`.
    pub fn offset(&self, ch: u16) -> f64 {
        let first = self.video.get(&ch).and_then(|f| f.first()).map_or(0, |f| f.0);
        first.saturating_sub(self.t0()) as f64 / 1000.0
    }

    /// Average frame rate of `ch`, from its timestamps (3 fps if unknown),
    /// clamped to a sane range so ffmpeg never gets `-r 0`.
    pub fn fps(&self, ch: u16) -> f64 {
        match self.video.get(&ch).map(Vec::as_slice) {
            Some(frames @ [first, .., last]) if last.0 > first.0 => {
                let fps = (frames.len() - 1) as f64 / ((last.0 - first.0) as f64 / 1000.0);
                fps.clamp(MIN_FPS, MAX_FPS)
            }
            _ => 3.0,
        }
    }

    /// Length (s) of channel `ch` once encoded at [`Self::fps`].
    pub fn channel_duration(&self, ch: u16) -> f64 {
        self.video.get(&ch).map_or(0.0, |f| f.len() as f64 / self.fps(ch))
    }

    /// Camera title for `ch`, cleaned for filenames and drawtext.
    pub fn label(&self, ch: u16) -> String {
        self.titles.get(&ch).map(|t| clean_title(t)).unwrap_or_default()
    }
}

fn parse_titles(header: &[u8]) -> BTreeMap<u16, String> {
    let text = String::from_utf8_lossy(header);
    text.split(['\n', '\0'])
        .filter_map(|line| {
            let i = line.find("CH")?;
            let (ch, title) = line[i + 2..].split_once(".Title=")?;
            Some((ch.parse().ok()?, title.trim().to_string()))
        })
        .collect()
}

/// Keep only alphanumerics, space, '-' and '_'.
pub fn clean_title(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tag: u8, seq: u32, ts: u32, ch: u16, payload: &[u8]) -> Vec<u8> {
        let hdr = if tag == VIDEO_TAG { VIDEO_HDR } else { AUDIO_HDR };
        let mut r = vec![0u8; hdr];
        r[..4].copy_from_slice(&[0xff, tag, 0x01, 0xe0]);
        r[4..8].copy_from_slice(&seq.to_le_bytes());
        r[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        r[12..14].copy_from_slice(&352u16.to_le_bytes());
        r[14..16].copy_from_slice(&288u16.to_le_bytes());
        r[16..20].copy_from_slice(&ts.to_le_bytes());
        r[20..22].copy_from_slice(&ch.to_le_bytes());
        r.extend_from_slice(payload);
        r
    }

    #[test]
    fn parses_synthetic_file() {
        let mut d = b"VA\0\0\0\0\0\0".to_vec();
        for v in [2015u16, 3, 2, 17, 9, 54, 59, 991] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.resize(0x40, 0);
        d.extend_from_slice(b"CH1.Title=FRONT/DOOR\nCH2.Title=GARDEN\n");
        d.extend(record(VIDEO_TAG, 1, 1000, 1, b"\0\0\0\x01AAAA"));
        d.extend(record(AUDIO_TAG, 2, 1000, 0, &[0x7f; 16]));
        d.extend(b"garbage\x01\xe0junk"); // must be skipped by resync
        d.extend(record(VIDEO_TAG, 3, 1500, 2, b"\0\0\0\x01BB"));
        d.extend(record(VIDEO_TAG, 4, 2000, 1, b"\0\0\0\x01CCCC"));
        let mut truncated = record(VIDEO_TAG, 5, 2500, 1, b"\0\0\0\x01DDDDDDDD");
        truncated.truncate(50);
        d.extend(truncated);

        let rec = Recording::parse(d).unwrap();
        assert_eq!(rec.created.unwrap().to_string(), "2015-03-17 09:54:59.991");
        assert!(rec.first_frame.is_none());
        assert_eq!(rec.titles[&1], "FRONT/DOOR");
        assert_eq!(rec.label(1), "FRONTDOOR");
        assert_eq!(rec.video[&1].len(), 2);
        assert_eq!(rec.video[&2].len(), 1);
        assert_eq!(rec.frame(&rec.video[&1][1].1), b"\0\0\0\x01CCCC");
        assert_eq!(rec.audio.len(), 1);
        assert_eq!(rec.frame(&rec.audio[0].1), &[0x7f; 16]);
        assert_eq!(rec.sizes[&1], (352, 288));
        assert_eq!(rec.offset(2), 0.5);
        assert_eq!(rec.duration(), 1.0);
        assert_eq!(rec.fps(1), 1.0);
    }

    #[test]
    fn rejects_non_avc() {
        assert!(Recording::parse(b"hello".to_vec()).is_err());
    }

    fn header() -> Vec<u8> {
        let mut d = b"VA".to_vec();
        d.resize(0x40, 0);
        d.extend_from_slice(b"CH1.Title=A\n");
        d
    }

    #[test]
    fn add_millis_carries() {
        let t = |y, mo, d, h, mi, s, ms| DvrTime { year: y, month: mo, day: d, hour: h, minute: mi, second: s, millis: ms };
        let a = t(2015, 3, 17, 9, 55, 12, 873);
        assert_eq!(a.add_millis(44_662).to_string(), "2015-03-17 09:55:57.535");
        assert_eq!(a.add_millis(0), a);
        assert_eq!(t(2016, 2, 28, 23, 59, 59, 900).add_millis(200).to_string(), "2016-02-29 00:00:00.100");
        assert_eq!(t(2015, 2, 28, 23, 59, 59, 900).add_millis(200).to_string(), "2015-03-01 00:00:00.100");
        assert_eq!(t(2015, 12, 31, 23, 0, 0, 0).add_millis(3_600_000).to_string(), "2016-01-01 00:00:00.000");
        assert_eq!(t(2015, 1, 30, 12, 0, 0, 0).add_millis(3 * 86_400_000).to_string(), "2015-02-02 12:00:00.000");
    }

    #[test]
    fn last_frame_is_first_frame_plus_span() {
        let mut d = b"VA".to_vec();
        d.resize(0x18, 0);
        for v in [2015u16, 3, 2, 17, 9, 55, 12, 873] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.resize(0x40, 0);
        d.extend(record(VIDEO_TAG, 1, 5000, 1, b"\0\0\0\x01"));
        d.extend(record(AUDIO_TAG, 2, 5077, 0, &[0; 4]));
        d.extend(record(VIDEO_TAG, 3, 9500, 2, b"\0\0\0\x01"));
        d.extend(record(AUDIO_TAG, 4, 9662, 0, &[0; 4]));
        let rec = Recording::parse(d).unwrap();
        assert!(rec.created.is_none());
        assert_eq!(rec.first_frame.unwrap().to_string(), "2015-03-17 09:55:12.873");
        assert_eq!(rec.last_frame().unwrap().to_string(), "2015-03-17 09:55:17.535");
    }

    #[test]
    fn rejects_empty() {
        assert!(Recording::parse(Vec::new()).is_err());
    }

    #[test]
    fn corrupt_size_resyncs_instead_of_stopping() {
        let mut d = header();
        d.extend(record(VIDEO_TAG, 1, 1000, 1, b"\0\0\0\x01AA"));
        let mut bad = record(VIDEO_TAG, 2, 1300, 1, b"\0\0\0\x01BB");
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        d.extend(bad);
        d.extend(record(VIDEO_TAG, 3, 1600, 1, b"\0\0\0\x01CC"));
        d.extend(record(AUDIO_TAG, 4, 1600, 0, &[0x7f; 8]));
        let rec = Recording::parse(d).unwrap();
        assert_eq!(rec.video[&1].len(), 2);
        assert_eq!(rec.frame(&rec.video[&1][1].1), b"\0\0\0\x01CC");
        assert_eq!(rec.audio.len(), 1);
    }

    #[test]
    fn timestamps_unwrap_across_u32_rollover() {
        let mut d = header();
        let t = u32::MAX - 499; // wraps 500 ms after the first frame
        for (i, ts) in [t, t.wrapping_add(500), t.wrapping_add(1000)].into_iter().enumerate() {
            d.extend(record(VIDEO_TAG, i as u32, ts, 1, b"\0\0\0\x01"));
        }
        // A second channel that starts right after the wrap.
        d.extend(record(VIDEO_TAG, 9, t.wrapping_add(1100), 2, b"\0\0\0\x01"));
        let rec = Recording::parse(d).unwrap();
        assert_eq!(rec.duration(), 1.1);
        assert_eq!(rec.fps(1), 2.0);
        assert_eq!(rec.offset(2), 1.1);
    }

    #[test]
    fn fps_is_clamped_and_defaults() {
        let mut d = header();
        // Two frames one hour apart: ~0.0003 fps would print as "-r 0.0000".
        d.extend(record(VIDEO_TAG, 1, 0, 1, b"\0\0\0\x01"));
        d.extend(record(VIDEO_TAG, 2, 3_600_000, 1, b"\0\0\0\x01"));
        // Two frames with the same timestamp.
        d.extend(record(VIDEO_TAG, 3, 10, 2, b"\0\0\0\x01"));
        d.extend(record(VIDEO_TAG, 4, 10, 2, b"\0\0\0\x01"));
        // A single frame.
        d.extend(record(VIDEO_TAG, 5, 10, 3, b"\0\0\0\x01"));
        let rec = Recording::parse(d).unwrap();
        assert_eq!(rec.fps(1), MIN_FPS);
        assert_eq!(rec.fps(2), 3.0);
        assert_eq!(rec.fps(3), 3.0);
        assert_eq!(rec.fps(4), 3.0); // missing channel
        assert!(rec.channel_duration(3) > 0.0);
    }

    #[test]
    fn bogus_header_times_are_rejected() {
        let mut d = b"VA\0\0\0\0\0\0".to_vec();
        for v in [2015u16, 3, 2, 17, 9, 99, 59, 991] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        assert!(Recording::parse(d).unwrap().created.is_none());
    }

    /// Every truncation of a valid file, and pseudo-random garbage behind a
    /// "VA" magic, must parse without panicking.
    #[test]
    fn never_panics_on_truncated_or_random_input() {
        let mut d = header();
        for i in 0..6u32 {
            d.extend(record(if i % 3 == 2 { AUDIO_TAG } else { VIDEO_TAG }, i, 1000 + i * 300, (i % 2 + 1) as u16, &[0x41; 20]));
        }
        for n in 0..=d.len() {
            let _ = Recording::parse(d[..n].to_vec());
        }
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..50 {
            let mut r = b"VA".to_vec();
            for _ in 0..4000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                // Bias towards marker bytes so records actually get parsed.
                r.push(match x % 8 {
                    0 => 0xff,
                    1 => 0x3d,
                    2 => 0x01,
                    3 => 0xe0,
                    _ => (x >> 8) as u8,
                });
            }
            if let Ok(rec) = Recording::parse(r) {
                for ch in rec.video.keys() {
                    assert!((MIN_FPS..=MAX_FPS).contains(&rec.fps(*ch)) || rec.fps(*ch) == 3.0);
                }
                let _ = rec.duration();
            }
        }
    }
}
