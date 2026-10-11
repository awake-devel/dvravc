//! Graphical UI (eframe/egui + rfd file dialogs).

use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;

use anyhow::Result;
use dvravc::convert::{self, Batch, Msg, Options};
use eframe::egui;

pub fn run() -> Result<()> {
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 560.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("dvravc", native, Box::new(|_cc| Ok(Box::new(App::new()))))
        .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

struct App {
    files: Vec<PathBuf>,
    out_dir: PathBuf,
    opts: Options,
    log: Vec<String>,
    worker: Option<Batch>,
    ffmpeg: bool,
}

impl App {
    fn new() -> Self {
        let out_dir = std::env::current_dir().unwrap_or_default().join("output");
        App { files: vec![], out_dir, opts: Options::default(), log: vec![], worker: None, ffmpeg: convert::ffmpeg_available() }
    }

    fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            // Dropped folders are scanned for .avc files.
            let found = if p.is_dir() { convert::scan_dir(&p).unwrap_or_default() } else { vec![p] };
            for f in found {
                if !self.files.contains(&f) {
                    self.files.push(f);
                }
            }
        }
    }

    fn poll_worker(&mut self) {
        let Some(b) = &self.worker else { return };
        loop {
            match b.rx.try_recv() {
                Ok(Msg::Log(s)) => self.log.push(s),
                Ok(Msg::Finished { ok, failed }) => {
                    self.log.push(format!("Done: {ok} ok, {failed} failed"));
                    self.worker = None;
                    return;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => return,
                // The worker died without saying so: don't stay "busy" forever.
                Err(TryRecvError::Disconnected) => {
                    self.log.push("Conversion stopped unexpectedly".into());
                    self.worker = None;
                    return;
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker();
        let busy = self.worker.is_some();
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add(dropped);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("DVR .avc to MP4");
            if !self.ffmpeg {
                ui.colored_label(egui::Color32::RED, convert::FFMPEG_HINT);
            }

            ui.horizontal(|ui| {
                if ui.button("Add files…").clicked()
                    && let Some(f) = rfd::FileDialog::new().add_filter("DVR recording", &["avc", "AVC"]).pick_files()
                {
                    self.add(f);
                }
                if ui.button("Clear").clicked() {
                    self.files.clear();
                }
                ui.label("…or drop .avc files / folders here");
            });

            egui::ScrollArea::vertical().id_salt("files").max_height(180.0).show(ui, |ui| {
                let mut remove = None;
                for (i, f) in self.files.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("✖").on_hover_text("Remove").clicked() {
                            remove = Some(i);
                        }
                        ui.label(f.display().to_string());
                    });
                }
                if let Some(i) = remove {
                    self.files.remove(i);
                }
                if self.files.is_empty() {
                    ui.weak("No files yet.");
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Output folder:");
                ui.monospace(self.out_dir.display().to_string());
                if ui.button("Choose…").clicked()
                    && let Some(d) = rfd::FileDialog::new().set_directory(&self.out_dir).pick_folder()
                {
                    self.out_dir = d;
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.opts.channels, "One MP4 per channel");
                ui.checkbox(&mut self.opts.mosaic, "2x2 mosaic");
            });

            let can_run = !busy && self.ffmpeg && !self.files.is_empty() && (self.opts.channels || self.opts.mosaic);
            ui.horizontal(|ui| {
                if ui.add_enabled(can_run, egui::Button::new("Convert")).clicked() {
                    self.log.push(format!("Converting {} file(s) to {}", self.files.len(), self.out_dir.display()));
                    self.worker = Some(convert::spawn_batch(self.files.clone(), self.out_dir.clone(), self.opts));
                }
                if busy {
                    ui.spinner();
                    ui.label("Converting…");
                }
            });

            ui.separator();
            ui.label("Log");
            egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                for line in &self.log {
                    ui.monospace(line);
                }
            });
        });
    }
}
