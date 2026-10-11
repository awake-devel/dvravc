use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use dvravc::{Options, Recording, convert};

mod gui;
mod tui;

#[derive(Parser)]
#[command(version, about = "Convert 4-channel DVR .avc surveillance recordings to MP4")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert .avc files (or directories containing them) to MP4.
    Convert {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Output root; each recording gets its own subfolder. Existing MP4s
        /// with the same names are overwritten.
        #[arg(short, long, default_value = "output")]
        output: PathBuf,
        /// Don't write one MP4 per channel.
        #[arg(long)]
        no_channels: bool,
        /// Don't write the 2x2 mosaic.
        #[arg(long)]
        no_mosaic: bool,
    },
    /// Show what a recording contains.
    Info { file: PathBuf },
    /// Terminal UI listing the .avc files in DIR (default: current dir).
    Tui { dir: Option<PathBuf> },
    /// Graphical UI.
    Gui,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Convert { inputs, output, no_channels, no_mosaic } => {
            let opts = Options { channels: !no_channels, mosaic: !no_mosaic };
            if !opts.channels && !opts.mosaic {
                bail!("nothing to do: both --no-channels and --no-mosaic given");
            }
            if !dvravc::ffmpeg_available() {
                bail!("{}", convert::FFMPEG_HINT);
            }
            let files = convert::collect_inputs(&inputs)?;
            if files.is_empty() {
                bail!("no .avc files found");
            }
            let mut failed = 0;
            let dirs = convert::output_dirs(&files, &output);
            for (f, dir) in files.iter().zip(&dirs) {
                let mut report = |e: convert::Event| {
                    if let convert::Event::Log(s) = e {
                        println!("{s}")
                    }
                };
                if let Err(e) = convert::convert_to(f, dir, &opts, &convert::Cancel::default(), &mut report) {
                    eprintln!("error: {}: {e:#}", f.display());
                    failed += 1;
                }
            }
            if failed > 0 {
                bail!("{failed} of {} file(s) failed", files.len());
            }
            Ok(())
        }
        Cmd::Info { file } => info(&file),
        Cmd::Tui { dir } => tui::run(dir.unwrap_or_else(|| PathBuf::from("."))),
        Cmd::Gui => gui::run(),
    }
}

fn info(file: &std::path::Path) -> Result<()> {
    let rec = Recording::open(file)?;
    println!("File:        {}", file.display());
    let fmt = |t: Option<dvravc::demux::DvrTime>| t.map_or("?".into(), |t| t.to_string());
    println!("Created:     {}", fmt(rec.created));
    println!("First frame: {}", fmt(rec.first_frame));
    println!("Last frame:  {} (estimated from frame timestamps)", fmt(rec.last_frame()));
    println!("Duration:    {:.1}s (video, from frame timestamps)", rec.duration());
    println!("Audio:       {} block(s) of G.711 mu-law 8 kHz", rec.audio.len());
    for (&ch, frames) in &rec.video {
        let (w, h) = rec.sizes.get(&ch).copied().unwrap_or_default();
        println!(
            "CH{ch} {:<12} {w}x{h}  {:>5} frames  {:>5.2} fps  starts +{:.1}s",
            rec.titles.get(&ch).map_or("", String::as_str),
            frames.len(),
            rec.fps(ch),
            rec.offset(ch)
        );
    }
    Ok(())
}
