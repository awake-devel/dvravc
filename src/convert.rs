//! Conversion of a demuxed recording to MP4 files using the ffmpeg CLI.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::demux::Recording;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Write one MP4 per camera channel.
    pub channels: bool,
    /// Write a 2x2 mosaic of all channels.
    pub mosaic: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { channels: true, mosaic: true }
    }
}

/// Progress reported by [`convert`] while it works on one file.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Log(String),
    /// `done` of `total` output files are finished.
    Step { done: usize, total: usize },
}

/// Messages sent by [`spawn_batch`]. `index` is the position in the input list.
#[derive(Debug)]
pub enum Msg {
    Log(String),
    Started { index: usize },
    Step { index: usize, done: usize, total: usize },
    /// One input is done; `error` is `None` on success.
    FileDone { index: usize, outputs: Vec<PathBuf>, error: Option<String> },
    Finished { ok: usize, failed: usize },
}

/// Shared flag that stops a running conversion (ffmpeg is killed).
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// True if an `ffmpeg` binary can be run.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Install hints shown when ffmpeg can't be found.
pub const FFMPEG_HINT: &str = "ffmpeg was not found on PATH. Install it:\n\
  Windows: winget install Gyan.FFmpeg   (or: choco install ffmpeg)\n\
  macOS:   brew install ffmpeg\n\
  Linux:   sudo apt install ffmpeg / sudo dnf install ffmpeg / sudo pacman -S ffmpeg";

/// `fontfile=...:` for drawtext. Windows and macOS ffmpeg builds often lack
/// fontconfig, so point at a known system font; Linux uses the fontconfig default.
fn font_option() -> String {
    let candidates: &[&str] = if cfg!(windows) {
        &["C:/Windows/Fonts/arial.ttf", "C:/Windows/Fonts/segoeui.ttf"]
    } else if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
            "/Library/Fonts/Arial.ttf",
        ]
    } else {
        &[]
    };
    candidates.iter().find(|p| Path::new(p).exists()).map(|p| font_arg(p)).unwrap_or_default()
}

/// The quotes are removed by the filtergraph parser, then `\:` by the option
/// parser, so `C:/x.ttf` reaches drawtext intact.
fn font_arg(path: &str) -> String {
    format!("fontfile='{}':", path.replace('\\', "/").replace(':', "\\:"))
}

/// A path as an ffmpeg argument. Relative paths could start with `-` (read as
/// an option) or contain `name:` (read as a protocol), so make them absolute.
fn path_arg(p: &Path) -> String {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).display().to_string()
}

/// Run ffmpeg with `args`; on failure return the tail of its stderr.
fn ffmpeg(args: &[String], cancel: &Cancel) -> Result<()> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to run ffmpeg: {e}\n{FFMPEG_HINT}"))?;
    // Drain stderr on a thread so a chatty ffmpeg can't fill the pipe and block.
    let mut stderr = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_end(&mut s);
        }
        s
    });
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            bail!("cancelled");
        }
        thread::sleep(Duration::from_millis(50));
    };
    let err = reader.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let lines: Vec<&str> = err.lines().collect();
        let tail = lines[lines.len().saturating_sub(10)..].join("\n");
        bail!("ffmpeg failed ({status}):\n{tail}");
    }
    Ok(())
}

/// Run ffmpeg to produce `out`, removing a partial file if it fails.
fn encode(args: &[String], out: &Path, cancel: &Cancel) -> Result<()> {
    let r = ffmpeg(args, cancel);
    if r.is_err() {
        let _ = std::fs::remove_file(out);
    }
    r
}

/// Concatenate the given payload ranges into a file.
fn write_stream<'a>(rec: &Recording, ranges: impl Iterator<Item = &'a std::ops::Range<usize>>, path: &Path) -> Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    for r in ranges {
        w.write_all(rec.frame(r))?;
    }
    w.flush()?;
    Ok(())
}

/// ffmpeg input arguments for the raw mu-law audio track, so that it starts
/// `offset` seconds after the video (or `-offset` seconds into the audio when
/// the offset is negative).
fn audio_input(path: &Path, offset: f64) -> Vec<String> {
    let mut a = vec![];
    if offset >= 0.001 {
        a.extend(["-itsoffset".into(), format!("{offset:.3}")]);
    } else if offset <= -0.001 {
        // A negative -itsoffset gives negative timestamps, which the MP4
        // muxer may shift or keep; skipping into the input is unambiguous.
        a.extend(["-ss".into(), format!("{:.3}", -offset)]);
    }
    a.extend(["-f", "mulaw", "-ar", "8000", "-ac", "1", "-i"].map(String::from));
    a.push(path_arg(path));
    a
}

/// Output folder for each input: `out_root/<file stem>`, with `_2`, `_3`, …
/// appended when two inputs share a stem, so they can't overwrite each other.
pub fn output_dirs(files: &[PathBuf], out_root: &Path) -> Vec<PathBuf> {
    let mut used = HashSet::new();
    files
        .iter()
        .map(|f| {
            let stem = f.file_stem().map(|s| s.to_string_lossy().into_owned());
            let stem = stem.filter(|s| !s.is_empty()).unwrap_or_else(|| "recording".into());
            let mut name = stem.clone();
            let mut n = 1;
            // Case-insensitive, as on Windows and macOS file systems.
            while !used.insert(name.to_lowercase()) {
                n += 1;
                name = format!("{stem}_{n}");
            }
            out_root.join(name)
        })
        .collect()
}

/// Convert one .avc file into `out_root/<file stem>/`.
pub fn convert(input: &Path, out_root: &Path, opts: &Options, log: &mut dyn FnMut(String)) -> Result<Vec<PathBuf>> {
    let out_dir = output_dirs(&[input.to_path_buf()], out_root).remove(0);
    convert_to(input, &out_dir, opts, &Cancel::default(), &mut |e| {
        if let Event::Log(s) = e {
            log(s)
        }
    })
}

/// Convert one .avc file into `out_dir`. Every requested output is attempted
/// even if an earlier one fails; the error lists the failures.
pub fn convert_to(
    input: &Path,
    out_dir: &Path,
    opts: &Options,
    cancel: &Cancel,
    report: &mut dyn FnMut(Event),
) -> Result<Vec<PathBuf>> {
    let rec = Recording::open(input)?;
    if rec.video.is_empty() {
        bail!("no video frames found in {}", input.display());
    }
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let tmp = tempfile::TempDir::new().context("creating a temporary folder")?;
    report(Event::Log(format!(
        "{}: {} channel(s), {:.1}s, {} audio block(s)",
        input.display(),
        rec.video.len(),
        rec.duration(),
        rec.audio.len()
    )));

    // Intermediates: raw H.264 per channel, raw mu-law audio.
    let audio = if rec.audio.is_empty() {
        None
    } else {
        let p = tmp.path().join("audio.ulaw");
        write_stream(&rec, rec.audio.iter().map(|(_, r)| r), &p)?;
        Some(p)
    };
    // Audio start relative to the first video frame (s).
    let audio_start = rec.audio.first().map_or(0.0, |a| (a.0 as f64 - rec.t0() as f64) / 1000.0);

    let mut h264 = Vec::new();
    for (&ch, frames) in &rec.video {
        let p = tmp.path().join(format!("ch{ch}.h264"));
        write_stream(&rec, frames.iter().map(|(_, r)| r), &p)?;
        h264.push((ch, p));
    }

    let total = if opts.channels { h264.len() } else { 0 } + usize::from(opts.mosaic);
    let mut done = 0;
    report(Event::Step { done, total });
    let mut outputs = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    if opts.channels {
        for (ch, raw) in &h264 {
            let (ch, fps, off) = (*ch, rec.fps(*ch), rec.offset(*ch));
            let label = rec.label(ch);
            let name = if label.is_empty() { format!("CH{ch}") } else { format!("CH{ch}_{label}") };
            let mp4 = out_dir.join(format!("{name}.mp4"));
            let mut args: Vec<String> = vec!["-r".into(), format!("{fps:.4}"), "-i".into(), path_arg(raw)];
            if let Some(a) = &audio {
                // Shift audio so it lines up with this channel's first frame.
                args.extend(audio_input(a, audio_start - off));
                args.extend(["-map", "0:v", "-map", "1:a", "-c:a", "aac"].map(String::from));
                // Cut to the video length. (-shortest would also cut the
                // video when the audio ends first, dropping the last frames.)
                args.extend(["-t".into(), format!("{:.3}", rec.channel_duration(ch))]);
            }
            args.extend(["-c:v", "libx264", "-pix_fmt", "yuv420p"].map(String::from));
            args.push(path_arg(&mp4));
            match encode(&args, &mp4, cancel) {
                Ok(()) => {
                    report(Event::Log(format!(
                        "  {}: {} frames, {fps:.2} fps, starts +{off:.1}s",
                        mp4.display(),
                        rec.video.get(&ch).map_or(0, Vec::len)
                    )));
                    outputs.push(mp4);
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        bail!("cancelled");
                    }
                    report(Event::Log(format!("  channel {ch} failed: {e:#}")));
                    errors.push(format!("channel {ch}"));
                }
            }
            done += 1;
            report(Event::Step { done, total });
        }
    }

    if opts.mosaic {
        let mp4 = out_dir.join("mosaic.mp4");
        let mut r = mosaic(&rec, &h264, audio.as_deref(), audio_start, true, &mp4, cancel);
        if let Err(e) = &r
            && !cancel.is_cancelled()
        {
            // Some ffmpeg builds lack drawtext or a usable font: retry without labels.
            report(Event::Log(format!("  mosaic with labels failed, retrying without: {e:#}")));
            r = mosaic(&rec, &h264, audio.as_deref(), audio_start, false, &mp4, cancel);
        }
        match r {
            Ok(()) => {
                report(Event::Log(format!("  {}: {:.1}s", mp4.display(), rec.duration())));
                outputs.push(mp4);
            }
            Err(_) if cancel.is_cancelled() => bail!("cancelled"),
            Err(e) => {
                report(Event::Log(format!("  mosaic failed: {e:#}")));
                errors.push("mosaic".into());
            }
        }
        done += 1;
        report(Event::Step { done, total });
    }
    if !errors.is_empty() {
        bail!("{} of {total} output(s) failed: {}", errors.len(), errors.join(", "));
    }
    Ok(outputs)
}

/// 2x2 mosaic aligned on wall-clock start, with channel labels.
fn mosaic(
    rec: &Recording,
    h264: &[(u16, PathBuf)],
    audio: Option<&Path>,
    audio_start: f64,
    labels: bool,
    out: &Path,
    cancel: &Cancel,
) -> Result<()> {
    let args = mosaic_args(rec, h264, audio, audio_start, labels.then(font_option), out);
    encode(&args, out, cancel).context("encoding mosaic")
}

/// ffmpeg arguments for [`mosaic`]. `font` is `None` for no labels.
fn mosaic_args(
    rec: &Recording,
    h264: &[(u16, PathBuf)],
    audio: Option<&Path>,
    audio_start: f64,
    font: Option<String>,
    out: &Path,
) -> Vec<String> {
    // All tiles are scaled to the first real channel size so xstack lines up
    // (libx264 needs even dimensions).
    let (w, h) = rec
        .sizes
        .values()
        .copied()
        .find(|&(w, h)| w >= 16 && h >= 16)
        .map_or((352, 288), |(w, h)| (w & !1, h & !1));
    let n = h264.len().min(4);
    let duration = rec.duration() + 0.3;
    let mut args: Vec<String> = Vec::new();
    let mut filt = Vec::new();
    for (i, (ch, raw)) in h264.iter().take(4).enumerate() {
        args.extend(["-r".into(), format!("{:.4}", rec.fps(*ch)), "-i".into(), path_arg(raw)]);
        let mut f = format!(
            "[{i}:v]setpts=PTS-STARTPTS,scale={w}:{h},\
             tpad=start_mode=clone:start_duration={off:.3}:stop_mode=clone:stop_duration={stop:.3}",
            off = rec.offset(*ch),
            stop = duration + 1.0,
        );
        if let Some(font) = &font {
            // The label is sanitised, so it needs no escaping inside quotes.
            let text = format!("CH{ch} {}", rec.label(*ch));
            f += &format!(
                ",drawtext={font}text='{text}':x=8:y=8:fontsize=18:fontcolor=white:box=1:boxcolor=black@0.5",
                text = text.trim()
            );
        }
        filt.push(format!("{f}[v{i}]"));
    }
    let stack: String = (0..n).map(|i| format!("[v{i}]")).collect();
    if n == 1 {
        filt.push(format!("{stack}null[v]"));
    } else {
        let layout = ["0_0", "w0_0", "0_h0", "w0_h0"][..n].join("|");
        filt.push(format!("{stack}xstack=inputs={n}:layout={layout}:fill=black[v]"));
    }
    if let Some(a) = audio {
        args.extend(audio_input(a, audio_start));
    }
    args.extend(["-filter_complex".into(), filt.join(";"), "-map".into(), "[v]".into()]);
    if audio.is_some() {
        args.extend(["-map".into(), format!("{n}:a"), "-c:a".into(), "aac".into()]);
    }
    args.extend(["-c:v", "libx264", "-r", "10", "-pix_fmt", "yuv420p", "-t"].map(String::from));
    // Include the last frame's duration so the tail isn't cut.
    args.push(format!("{duration:.3}"));
    args.push(path_arg(out));
    args
}

/// A running [`spawn_batch`]: progress messages and a cancel handle.
pub struct Batch {
    pub rx: Receiver<Msg>,
    pub cancel: Cancel,
}

/// Convert `files` one after another on a background thread, reporting progress
/// over a channel. The last message is always [`Msg::Finished`], even if a
/// conversion panics.
pub fn spawn_batch(files: Vec<PathBuf>, out_root: PathBuf, opts: Options) -> Batch {
    let (tx, rx) = mpsc::channel();
    let cancel = Cancel::default();
    let flag = cancel.clone();
    thread::spawn(move || {
        let (mut ok, mut failed) = (0, 0);
        let dirs = output_dirs(&files, &out_root);
        for (index, (f, dir)) in files.iter().zip(&dirs).enumerate() {
            if flag.is_cancelled() {
                let _ = tx.send(Msg::FileDone { index, outputs: vec![], error: Some("cancelled".into()) });
                failed += 1;
                continue;
            }
            let _ = tx.send(Msg::Started { index });
            let mut report = |e: Event| {
                let _ = tx.send(match e {
                    Event::Log(s) => Msg::Log(s),
                    Event::Step { done, total } => Msg::Step { index, done, total },
                });
            };
            let res = catch_unwind(AssertUnwindSafe(|| convert_to(f, dir, &opts, &flag, &mut report)))
                .unwrap_or_else(|p| {
                    let what = p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    Err(anyhow::anyhow!("internal error: {what}"))
                });
            let msg = match res {
                Ok(outputs) => {
                    ok += 1;
                    Msg::FileDone { index, outputs, error: None }
                }
                Err(e) => {
                    failed += 1;
                    let _ = tx.send(Msg::Log(format!("ERROR {}: {e:#}", f.display())));
                    Msg::FileDone { index, outputs: vec![], error: Some(format!("{e:#}")) }
                }
            };
            let _ = tx.send(msg);
        }
        let _ = tx.send(Msg::Finished { ok, failed });
    });
    Batch { rx, cancel }
}

/// Expand `inputs`: files are kept as-is, directories are scanned for `*.avc`.
/// Duplicates are dropped.
pub fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for p in inputs {
        let found = if p.is_dir() { scan_dir(p)? } else { vec![p.clone()] };
        for f in found {
            if !files.contains(&f) {
                files.push(f);
            }
        }
    }
    Ok(files)
}

/// Sorted list of `*.avc` files directly inside `dir` (not recursive).
pub fn scan_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("avc")))
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_dirs_are_unique() {
        let files: Vec<PathBuf> =
            ["a/rec.avc", "b/rec.avc", "c/REC.avc", "d/other.avc", ".avc"].iter().map(PathBuf::from).collect();
        let dirs = output_dirs(&files, Path::new("out"));
        let names: Vec<String> = dirs.iter().map(|d| d.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["rec", "rec_2", "REC_3", "other", ".avc"]);
    }

    #[test]
    fn path_args_cant_be_options_or_protocols() {
        for p in ["-dash/x.mp4", "proto:x/y.mp4", "plain.mp4"] {
            let a = path_arg(Path::new(p));
            assert!(Path::new(&a).is_absolute(), "{a}");
            assert!(!a.starts_with('-'));
        }
    }

    #[test]
    fn audio_offset_uses_seek_when_negative() {
        let a = audio_input(Path::new("/t/a.ulaw"), -4.4);
        assert_eq!(a[..2], ["-ss", "4.400"]);
        let a = audio_input(Path::new("/t/a.ulaw"), 0.9);
        assert_eq!(a[..2], ["-itsoffset", "0.900"]);
        let a = audio_input(Path::new("/t/a.ulaw"), 0.0);
        assert_eq!(a[0], "-f");
    }

    #[test]
    fn font_path_is_escaped_for_filtergraph() {
        assert_eq!(font_arg("C:/Windows/Fonts/arial.ttf"), "fontfile='C\\:/Windows/Fonts/arial.ttf':");
        assert_eq!(font_arg("C:\\Windows\\Fonts\\a.ttf"), "fontfile='C\\:/Windows/Fonts/a.ttf':");
    }

    /// Recording with `n` channels, the second one starting 0.5 s late.
    fn synthetic(n: u16, audio: bool) -> Recording {
        let mut rec = Recording::default();
        for ch in 1..=n {
            let start = if ch == 2 { 1500 } else { 1000 };
            rec.video.insert(ch, (0..4).map(|i| (start + i * 300, 0..0)).collect());
            rec.sizes.insert(ch, (352, 288));
            rec.titles.insert(ch, format!("Cam {ch}"));
        }
        if audio {
            rec.audio.push((1000, 0..0));
        }
        rec
    }

    #[test]
    fn mosaic_filter_graph_for_1_to_4_channels() {
        for n in 1..=4u16 {
            for audio in [false, true] {
                let rec = synthetic(n, audio);
                let h264: Vec<_> = (1..=n).map(|c| (c, PathBuf::from(format!("/t/ch{c}.h264")))).collect();
                let a = audio.then(|| Path::new("/t/a.ulaw"));
                let args = mosaic_args(&rec, &h264, a, 0.0, Some(String::new()), Path::new("/o/m.mp4"));
                let i = args.iter().position(|s| s == "-filter_complex").unwrap();
                let graph = &args[i + 1];
                assert_eq!(graph.matches("drawtext").count(), n as usize);
                if n == 1 {
                    assert!(graph.ends_with("[v0]null[v]"), "{graph}");
                } else {
                    assert!(graph.contains(&format!("xstack=inputs={n}:")), "{graph}");
                }
                assert_eq!(args.iter().filter(|s| *s == "-i").count(), n as usize + usize::from(audio));
                assert_eq!(args.contains(&format!("{n}:a")), audio);
                assert_eq!(args.last().unwrap(), &path_arg(Path::new("/o/m.mp4")));
            }
        }
    }
}
