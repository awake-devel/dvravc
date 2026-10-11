//! Parser for the proprietary 4-channel DVR ".avc" container.
//!
//! Layout (reverse-engineered, little-endian):
//! - file header starting with "VA", two Windows SYSTEMTIME values at 0x08
//!   (start) and 0x18 (end), then `CHn.Title=NAME\n` lines;
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
        let valid = (1..=12).contains(&t.month) && (1..=31).contains(&t.day) && t.hour < 24;
        valid.then_some(t)
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
    pub start: Option<DvrTime>,
    pub end: Option<DvrTime>,
    /// Channel number (1..4) -> `(timestamp ms, payload range)` of each H.264 frame.
    pub video: BTreeMap<u16, Vec<(u32, Range<usize>)>>,
    /// Channel number -> `(width, height)` of its first frame.
    pub sizes: BTreeMap<u16, (u16, u16)>,
    /// `(timestamp ms, payload range)` of each G.711 mu-law audio block.
    pub audio: Vec<(u32, Range<usize>)>,
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
            start: DvrTime::read(&data, 0x08),
            end: DvrTime::read(&data, 0x18),
            ..Default::default()
        };

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
            let payload = o + hdr..o + hdr + size as usize;
            if payload.end > d.len() {
                break;
            }
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
    pub fn t0(&self) -> u32 {
        self.video.values().filter_map(|f| f.first()).map(|f| f.0).min().unwrap_or(0)
    }

    /// Recording span in seconds, from the first to the last video frame.
    pub fn duration(&self) -> f64 {
        let last = self.video.values().filter_map(|f| f.last()).map(|f| f.0).max().unwrap_or(0);
        last.saturating_sub(self.t0()) as f64 / 1000.0
    }

    /// Delay (s) between the recording start and the first frame of `ch`.
    pub fn offset(&self, ch: u16) -> f64 {
        let first = self.video.get(&ch).and_then(|f| f.first()).map_or(0, |f| f.0);
        first.saturating_sub(self.t0()) as f64 / 1000.0
    }

    /// Average frame rate of `ch`, from its timestamps (3 fps if unknown).
    pub fn fps(&self, ch: u16) -> f64 {
        match self.video.get(&ch).map(Vec::as_slice) {
            Some([first, .., last]) if last.0 > first.0 => {
                (self.video[&ch].len() - 1) as f64 / ((last.0 - first.0) as f64 / 1000.0)
            }
            _ => 3.0,
        }
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
        assert_eq!(rec.start.unwrap().to_string(), "2015-03-17 09:54:59.991");
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
}
