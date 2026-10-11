//! Conversion of a demuxed recording to MP4 files using the ffmpeg CLI.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

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

/// Messages sent by [`spawn_batch`].
#[derive(Debug)]
pub enum Msg {
    Log(String),
    Finished { ok: usize, failed: usize },
}

/// True if an `ffmpeg` binary can be run.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
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
    candidates
        .iter()
        .find(|p| Path::new(p).exists())
        // Escape the drive colon ("C:") for the filter graph syntax.
        .map(|p| format!("fontfile='{}':", p.replace(':', "\\:")))
        .unwrap_or_default()
}

/// Run ffmpeg with `args`; on failure return the tail of its stderr.
fn ffmpeg(args: &[String]) -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run ffmpeg: {e}\n{FFMPEG_HINT}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = err.lines().collect();
        let tail = lines[lines.len().saturating_sub(10)..].join("\n");
        bail!("ffmpeg failed ({}):\n{tail}", out.status);
    }
    Ok(())
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

/// ffmpeg input arguments for the raw mu-law audio track, shifted by `offset` seconds.
fn audio_input(path: &Path, offset: f64) -> Vec<String> {
    let mut a = vec![];
    if offset.abs() >= 0.001 {
        a.extend(["-itsoffset".into(), format!("{offset:.3}")]);
    }
    a.extend(["-f", "mulaw", "-ar", "8000", "-ac", "1", "-i"].map(String::from));
    a.push(path.display().to_string());
    a
}

/// Convert one .avc file. Outputs go to `out_root/<file stem>/`.
pub fn convert(input: &Path, out_root: &Path, opts: &Options, log: &mut dyn FnMut(String)) -> Result<Vec<PathBuf>> {
    let rec = Recording::open(input)?;
    if rec.video.is_empty() {
        bail!("no video frames found in {}", input.display());
    }
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let out_dir = out_root.join(stem.as_ref());
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let tmp = tempfile::TempDir::new()?;
    log(format!(
        "{}: {} channel(s), {:.1}s, {} audio block(s)",
        input.display(),
        rec.video.len(),
        rec.duration(),
        rec.audio.len()
    ));

    // Intermediates: raw H.264 per channel, raw mu-law audio.
    let audio = if rec.audio.is_empty() {
        None
    } else {
        let p = tmp.path().join("audio.ulaw");
        write_stream(&rec, rec.audio.iter().map(|(_, r)| r), &p)?;
        Some(p)
    };
    // Audio start relative to the first video frame (s).
    let audio_start = rec.audio.first().map_or(0.0, |a| a.0 as f64 / 1000.0) - rec.t0() as f64 / 1000.0;

    let mut h264 = Vec::new();
    for (&ch, frames) in &rec.video {
        let p = tmp.path().join(format!("ch{ch}.h264"));
        write_stream(&rec, frames.iter().map(|(_, r)| r), &p)?;
        h264.push((ch, p));
    }

    let mut outputs = Vec::new();
    if opts.channels {
        for (ch, raw) in &h264 {
            let (ch, fps, off) = (*ch, rec.fps(*ch), rec.offset(*ch));
            let label = rec.label(ch);
            let name = if label.is_empty() { format!("CH{ch}") } else { format!("CH{ch}_{label}") };
            let mp4 = out_dir.join(format!("{name}.mp4"));
            let mut args: Vec<String> = vec!["-r".into(), format!("{fps:.4}"), "-i".into(), raw.display().to_string()];
            if let Some(a) = &audio {
                // Shift audio so it lines up with this channel's first frame.
                args.extend(audio_input(a, audio_start - off));
                args.extend(["-map", "0:v", "-map", "1:a", "-c:a", "aac", "-shortest"].map(String::from));
            }
            args.extend(["-c:v", "libx264", "-pix_fmt", "yuv420p"].map(String::from));
            args.push(mp4.display().to_string());
            ffmpeg(&args).with_context(|| format!("encoding channel {ch}"))?;
            log(format!(
                "  {}: {} frames, {fps:.2} fps, starts +{off:.1}s",
                mp4.display(),
                rec.video[&ch].len()
            ));
            outputs.push(mp4);
        }
    }

    if opts.mosaic {
        let mp4 = out_dir.join("mosaic.mp4");
        if let Err(e) = mosaic(&rec, &h264, audio.as_deref(), audio_start, true, &mp4) {
            // Some ffmpeg builds lack drawtext or a usable font: retry without labels.
            log(format!("  mosaic with labels failed, retrying without: {e:#}"));
            mosaic(&rec, &h264, audio.as_deref(), audio_start, false, &mp4)?;
        }
        log(format!("  {}: {:.1}s", mp4.display(), rec.duration()));
        outputs.push(mp4);
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
) -> Result<()> {
    // All tiles are scaled to the first channel's size so xstack lines up.
    let (w, h) = rec.sizes.values().next().copied().unwrap_or((352, 288));
    let n = h264.len().min(4);
    let mut args: Vec<String> = Vec::new();
    let mut filt = Vec::new();
    for (i, (ch, raw)) in h264.iter().take(4).enumerate() {
        args.extend(["-r".into(), format!("{:.4}", rec.fps(*ch)), "-i".into(), raw.display().to_string()]);
        let mut f = format!(
            "[{i}:v]setpts=PTS-STARTPTS,scale={w}:{h},\
             tpad=start_mode=clone:start_duration={off:.3}:stop_mode=clone:stop_duration=3600",
            off = rec.offset(*ch),
        );
        if labels {
            // The label is sanitised, so it needs no escaping inside quotes.
            let text = format!("CH{ch} {}", rec.label(*ch));
            f += &format!(
                ",drawtext={font}text='{text}':x=8:y=8:fontsize=18:fontcolor=white:box=1:boxcolor=black@0.5",
                font = font_option(),
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
    args.push(format!("{:.3}", rec.duration() + 0.3));
    args.push(out.display().to_string());
    ffmpeg(&args).context("encoding mosaic")
}

/// Convert `files` one after another on a background thread, reporting progress over a channel.
pub fn spawn_batch(files: Vec<PathBuf>, out_root: PathBuf, opts: Options) -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut ok, mut failed) = (0, 0);
        for f in &files {
            let tx2 = tx.clone();
            let mut log = |s: String| {
                let _ = tx2.send(Msg::Log(s));
            };
            match convert(f, &out_root, &opts, &mut log) {
                Ok(_) => ok += 1,
                Err(e) => {
                    failed += 1;
                    log(format!("ERROR {}: {e:#}", f.display()));
                }
            }
        }
        let _ = tx.send(Msg::Finished { ok, failed });
    });
    rx
}

/// Expand `inputs`: files are kept as-is, directories are scanned for `*.avc`.
pub fn collect_inputs(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for p in inputs {
        if p.is_dir() {
            files.extend(scan_dir(p)?);
        } else {
            files.push(p.clone());
        }
    }
    Ok(files)
}

/// Sorted list of `*.avc` files directly inside `dir`.
pub fn scan_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("avc")))
        .collect();
    files.sort();
    Ok(files)
}
