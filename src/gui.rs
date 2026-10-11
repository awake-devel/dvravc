//! Graphical UI (eframe/egui + rfd file dialogs).
//!
//! Conversion runs on [`convert::spawn_batch`]'s worker thread and file info
//! is parsed on a separate thread, so the UI thread never blocks on I/O.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use anyhow::Result;
use dvravc::convert::{self, Batch, Msg, Options};
use dvravc::demux::{DvrTime, Recording};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Pos2, Rect, RichText, Sense, Shape,
    Stroke, StrokeKind, TextStyle, Theme, ThemePreference, Vec2,
};

pub fn run(files: Vec<PathBuf>) -> Result<()> {
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("dvravc · DVR recordings to MP4")
            .with_app_id("dvravc")
            .with_inner_size([900.0, 760.0])
            .with_min_inner_size([620.0, 540.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "dvravc",
        native,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            // Hidden, for screenshots: DVRAVC_GUI_THEME=light|dark.
            match std::env::var("DVRAVC_GUI_THEME").as_deref() {
                Ok("light") => cc.egui_ctx.set_theme(ThemePreference::Light),
                Ok("dark") => cc.egui_ctx.set_theme(ThemePreference::Dark),
                _ => {}
            }
            let mut app = App::new(cc.egui_ctx.clone());
            app.add(files);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

mod theme {
    use super::*;

    /// Colours used by the custom-painted widgets; one set per theme.
    #[derive(Clone, Copy)]
    pub struct Pal {
        pub bg: Color32,
        pub well: Color32,
        pub surface: Color32,
        pub surface_hi: Color32,
        pub stroke: Color32,
        pub stroke_hi: Color32,
        pub text: Color32,
        pub dim: Color32,
        pub faint: Color32,
        pub accent: Color32,
        pub accent_hi: Color32,
        pub on_accent: Color32,
        pub ok: Color32,
        pub err: Color32,
        pub warn: Color32,
    }

    const fn hex(c: u32) -> Color32 {
        Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
    }

    pub const DARK: Pal = Pal {
        bg: hex(0x0b0d10),
        well: hex(0x07090b),
        surface: hex(0x13171c),
        surface_hi: hex(0x1a1f26),
        stroke: hex(0x232a33),
        stroke_hi: hex(0x323c48),
        text: hex(0xe8ecf1),
        dim: hex(0x9aa5b3),
        faint: hex(0x5d6874),
        accent: hex(0x2dd4bf),
        accent_hi: hex(0x5eead4),
        on_accent: hex(0x03201c),
        ok: hex(0x34d399),
        err: hex(0xf87171),
        warn: hex(0xfbbf24),
    };

    pub const LIGHT: Pal = Pal {
        bg: hex(0xf3f5f7),
        well: hex(0xe9edf1),
        surface: hex(0xffffff),
        surface_hi: hex(0xf1f4f7),
        stroke: hex(0xdfe4ea),
        stroke_hi: hex(0xc4ccd5),
        text: hex(0x101820),
        dim: hex(0x4d5866),
        faint: hex(0x8b95a1),
        accent: hex(0x0d9488),
        accent_hi: hex(0x0f766e),
        on_accent: hex(0xffffff),
        ok: hex(0x059669),
        err: hex(0xdc2626),
        warn: hex(0xb45309),
    };

    pub fn pal(ui: &egui::Ui) -> Pal {
        if ui.visuals().dark_mode { DARK } else { LIGHT }
    }

    fn visuals(p: Pal, dark: bool) -> egui::Visuals {
        let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.panel_fill = p.bg;
        v.window_fill = p.surface;
        v.extreme_bg_color = p.well;
        v.faint_bg_color = p.surface_hi;
        v.code_bg_color = p.surface_hi;
        v.window_stroke = Stroke::new(1.0_f32, p.stroke);
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(10);
        let shadow = egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if dark { 120 } else { 40 }) };
        v.window_shadow = shadow;
        v.popup_shadow = shadow;
        v.selection.bg_fill = p.accent.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0_f32, p.accent);
        v.hyperlink_color = p.accent;
        v.error_fg_color = p.err;
        v.warn_fg_color = p.warn;
        let r = CornerRadius::same(8);
        let w = &mut v.widgets;
        for (wv, bg, stroke, fg) in [
            (&mut w.noninteractive, p.surface, p.stroke, p.dim),
            (&mut w.inactive, p.surface_hi, p.stroke, p.text),
            (&mut w.hovered, p.surface_hi, p.stroke_hi, p.text),
            (&mut w.active, p.surface_hi, p.accent, p.text),
            (&mut w.open, p.surface_hi, p.stroke_hi, p.text),
        ] {
            wv.bg_fill = bg;
            wv.weak_bg_fill = bg;
            wv.bg_stroke = Stroke::new(1.0_f32, stroke);
            wv.fg_stroke = Stroke::new(1.0_f32, fg);
            wv.corner_radius = r;
            wv.expansion = 0.0;
        }
        // Hovered buttons lift a little.
        w.hovered.weak_bg_fill = if dark { hex(0x212833) } else { hex(0xe8edf2) };
        w.hovered.bg_fill = w.hovered.weak_bg_fill;
        w.active.weak_bg_fill = if dark { hex(0x283140) } else { hex(0xdde4eb) };
        v
    }

    fn style(s: &mut egui::Style) {
        use FontFamily::{Monospace, Proportional};
        s.text_styles = [
            (TextStyle::Heading, FontId::new(22.0, Proportional)),
            (TextStyle::Body, FontId::new(15.0, Proportional)),
            (TextStyle::Button, FontId::new(14.5, Proportional)),
            (TextStyle::Small, FontId::new(12.5, Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, Monospace)),
        ]
        .into();
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 7.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.window_margin = Margin::same(14);
        s.spacing.scroll = egui::style::ScrollStyle::floating();
        s.interaction.selectable_labels = false;
    }

    pub fn install(ctx: &egui::Context) {
        ctx.set_visuals_of(Theme::Dark, visuals(DARK, true));
        ctx.set_visuals_of(Theme::Light, visuals(LIGHT, false));
        ctx.all_styles_mut(style);
    }
}

use theme::Pal;

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// What the GUI shows about a recording, parsed off the UI thread.
struct Summary {
    time: Option<DvrTime>,
    duration: f64,
    channels: Vec<(u16, String)>,
    audio: bool,
}

enum Info {
    Loading,
    Ok(Summary),
    Err(String),
}

#[derive(Clone)]
enum Status {
    Idle,
    Queued,
    Running { done: usize, total: usize },
    Done { dir: PathBuf },
    Failed(String),
}

struct Item {
    path: PathBuf,
    size: Option<u64>,
    info: Info,
    status: Status,
}

impl Item {
    fn name(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into_owned())
    }
}

/// Progress of the current or last batch.
struct Run {
    batch: Option<Batch>,
    /// Batch index -> index in `App::items`.
    jobs: Vec<usize>,
    ok: usize,
    failed: usize,
    out_dir: PathBuf,
}

struct App {
    items: Vec<Item>,
    /// Output folder picked by the user; otherwise next to the first input.
    out_dir: Option<PathBuf>,
    opts: Options,
    log: Vec<String>,
    log_open: bool,
    run: Option<Run>,
    /// Scroll the result banner into view once a batch finishes.
    scroll_to_result: bool,
    shot_requested: bool,
    ffmpeg: bool,
    info_tx: Sender<PathBuf>,
    info_rx: Receiver<(PathBuf, Info)>,
    /// Drag and drop is not implemented by winit on Wayland.
    can_drop: bool,
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let (info_tx, req_rx) = mpsc::channel::<PathBuf>();
        let (res_tx, info_rx) = mpsc::channel();
        // One parser thread: dozens of dropped files are read one at a time.
        std::thread::spawn(move || {
            for path in req_rx {
                let info = match Recording::open(&path) {
                    Ok(rec) => Info::Ok(Summary {
                        time: rec.first_frame.or(rec.created),
                        duration: rec.duration(),
                        channels: rec.video.keys().map(|&c| (c, rec.label(c))).collect(),
                        audio: !rec.audio.is_empty(),
                    }),
                    Err(e) => Info::Err(format!("{:#}", e.root_cause())),
                };
                if res_tx.send((path, info)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        let ffmpeg = convert::ffmpeg_available();
        let mut log = vec![];
        if !ffmpeg {
            log.extend(convert::FFMPEG_HINT.lines().map(String::from));
        }
        App {
            items: vec![],
            out_dir: None,
            opts: Options::default(),
            log,
            log_open: false,
            run: None,
            scroll_to_result: false,
            shot_requested: false,
            ffmpeg,
            info_tx,
            info_rx,
            can_drop: !(cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())),
        }
    }

    fn busy(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.batch.is_some())
    }

    fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            // Dropped folders are scanned for .avc files.
            let found = if p.is_dir() {
                match convert::scan_dir(&p) {
                    Ok(f) => f,
                    Err(e) => {
                        self.log.push(format!("{e:#}"));
                        vec![]
                    }
                }
            } else {
                vec![p]
            };
            for f in found {
                let f = std::path::absolute(&f).unwrap_or(f);
                if self.items.iter().any(|i| i.path == f) {
                    continue;
                }
                let _ = self.info_tx.send(f.clone());
                self.items.push(Item {
                    size: std::fs::metadata(&f).ok().map(|m| m.len()),
                    path: f,
                    info: Info::Loading,
                    status: Status::Idle,
                });
            }
        }
    }

    fn out_dir(&self) -> Option<PathBuf> {
        self.out_dir.clone().or_else(|| {
            let first = self.items.first()?;
            let parent = first.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            Some(parent.join("output"))
        })
    }

    fn start(&mut self) {
        let Some(out_dir) = self.out_dir() else { return };
        let files: Vec<PathBuf> = self.items.iter().map(|i| i.path.clone()).collect();
        for i in &mut self.items {
            i.status = Status::Queued;
        }
        self.log.push(format!("Converting {} file(s) to {}", files.len(), out_dir.display()));
        let jobs = (0..files.len()).collect();
        self.run = Some(Run {
            batch: Some(convert::spawn_batch(files, out_dir.clone(), self.opts)),
            jobs,
            ok: 0,
            failed: 0,
            out_dir,
        });
    }

    fn poll(&mut self) {
        while let Ok((path, info)) = self.info_rx.try_recv() {
            if let Some(i) = self.items.iter_mut().find(|i| i.path == path) {
                i.info = info;
            }
        }
        let Some(run) = &mut self.run else { return };
        let Some(batch) = &run.batch else { return };
        loop {
            let msg = match batch.rx.try_recv() {
                Ok(m) => m,
                Err(TryRecvError::Empty) => return,
                // The worker died without saying so: don't stay "busy" forever.
                Err(TryRecvError::Disconnected) => {
                    self.log.push("Conversion stopped unexpectedly".into());
                    for i in &mut self.items {
                        if matches!(i.status, Status::Queued | Status::Running { .. }) {
                            i.status = Status::Failed("stopped unexpectedly".into());
                        }
                    }
                    run.batch = None;
                    return;
                }
            };
            let item = |index: usize| run.jobs.get(index).copied();
            match msg {
                Msg::Log(s) => self.log.extend(s.lines().map(String::from)),
                Msg::Started { index } => {
                    if let Some(it) = item(index).and_then(|i| self.items.get_mut(i)) {
                        it.status = Status::Running { done: 0, total: 1 };
                    }
                }
                Msg::Step { index, done, total } => {
                    if let Some(it) = item(index).and_then(|i| self.items.get_mut(i)) {
                        it.status = Status::Running { done, total: total.max(1) };
                    }
                }
                Msg::FileDone { index, outputs, error } => {
                    if let Some(it) = item(index).and_then(|i| self.items.get_mut(i)) {
                        it.status = match error {
                            None => Status::Done {
                                dir: outputs.first().and_then(|o| o.parent()).map_or(run.out_dir.clone(), Path::to_path_buf),
                            },
                            Some(e) => Status::Failed(e),
                        };
                    }
                }
                Msg::Finished { ok, failed } => {
                    self.log.push(format!("Done: {ok} ok, {failed} failed"));
                    run.ok = ok;
                    run.failed = failed;
                    self.scroll_to_result = true;
                    run.batch = None;
                    return;
                }
            }
        }
    }

    /// Overall progress of the running batch, 0..=1.
    fn progress(&self) -> f32 {
        let Some(run) = &self.run else { return 0.0 };
        if run.jobs.is_empty() {
            return 0.0;
        }
        let sum: f32 = run
            .jobs
            .iter()
            .filter_map(|&i| self.items.get(i))
            .map(|i| match i.status {
                Status::Running { done, total } => done as f32 / total as f32,
                Status::Done { .. } | Status::Failed(_) => 1.0,
                _ => 0.0,
            })
            .sum();
        sum / run.jobs.len() as f32
    }
}

impl Drop for App {
    /// Closing the window mid-batch kills ffmpeg and removes the partial file.
    fn drop(&mut self) {
        if let Some(b) = self.run.as_mut().and_then(|r| r.batch.take()) {
            b.cancel.cancel();
            while let Ok(m) = b.rx.recv_timeout(Duration::from_secs(5)) {
                if matches!(m, Msg::Finished { .. }) {
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    let mut child = cmd.arg(path).spawn()?;
    // Reap it without blocking the UI.
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn human_size(n: u64) -> String {
    let n = n as f64;
    match n {
        n if n >= 1e9 => format!("{:.1} GB", n / 1e9),
        n if n >= 1e6 => format!("{:.1} MB", n / 1e6),
        n if n >= 1e3 => format!("{:.0} kB", n / 1e3),
        n => format!("{n} B"),
    }
}

fn human_duration(s: f64) -> String {
    let s = s.max(0.0);
    if s < 60.0 {
        format!("{s:.1} s")
    } else {
        let t = s.round() as u64;
        if t >= 3600 { format!("{}h{:02}m", t / 3600, t / 60 % 60) } else { format!("{}m{:02}s", t / 60, t % 60) }
    }
}

fn card(p: Pal) -> egui::Frame {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.stroke))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(16))
}

/// Small uppercase section label.
fn section_label(ui: &mut egui::Ui, p: Pal, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.5).color(p.faint).extra_letter_spacing(1.2));
}

/// Rounded pill with a coloured dot (painted, so it stays compact in any layout).
fn pill(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), FontId::proportional(12.5), color);
    let size = Vec2::new(galley.size().x + 35.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(255),
        color.gamma_multiply(0.12),
        Stroke::new(1.0_f32, color.gamma_multiply(0.45)),
        StrokeKind::Inside,
    );
    painter.circle_filled(Pos2::new(rect.left() + 14.0, rect.center().y), 3.5, color);
    painter.galley(Pos2::new(rect.left() + 25.0, rect.center().y - galley.size().y / 2.0), galley, color);
    resp
}

/// iOS-style switch.
fn toggle(ui: &mut egui::Ui, p: Pal, on: &mut bool) -> egui::Response {
    let size = Vec2::new(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let off_bg = if resp.hovered() { p.stroke_hi } else { p.stroke };
    let bg = lerp_color(off_bg, p.accent, t);
    ui.painter().rect_filled(rect, CornerRadius::same(255), bg);
    let r = rect.height() / 2.0 - 3.0;
    let x = egui::lerp((rect.left() + r + 3.0)..=(rect.right() - r - 3.0), t);
    let knob = if *on { p.on_accent } else { p.text };
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), r, lerp_color(knob, Color32::WHITE, 0.15 * t));
    resp
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// Points around a rounded rectangle, for dashed outlines.
fn rounded_path(rect: Rect, r: f32) -> Vec<Pos2> {
    let mut pts = Vec::new();
    let corners = [
        (Pos2::new(rect.right() - r, rect.top() + r), -90.0f32),
        (Pos2::new(rect.right() - r, rect.bottom() - r), 0.0),
        (Pos2::new(rect.left() + r, rect.bottom() - r), 90.0),
        (Pos2::new(rect.left() + r, rect.top() + r), 180.0),
    ];
    for (c, start) in corners {
        for k in 0..=8 {
            let a = (start + 90.0 * k as f32 / 8.0).to_radians();
            pts.push(c + r * Vec2::new(a.cos(), a.sin()));
        }
    }
    pts.push(pts[0]);
    pts
}

/// Thin rounded progress bar.
fn bar(ui: &mut egui::Ui, p: Pal, frac: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let cr = CornerRadius::same(255);
    ui.painter().rect_filled(rect, cr, p.stroke);
    let frac = ui.ctx().animate_value_with_time(ui.id().with("bar").with(rect.min.y as i32), frac.clamp(0.0, 1.0), 0.25);
    if frac > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * frac).max(height));
        ui.painter().rect_filled(fill, cr, p.accent);
    }
}

/// Large filled call-to-action button.
fn big_button(ui: &mut egui::Ui, p: Pal, text: &str, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), if enabled { Sense::click() } else { Sense::hover() });
    let t = ui.ctx().animate_bool_responsive(resp.id, enabled && resp.hovered());
    let (fill, fg) = if enabled { (lerp_color(p.accent, p.accent_hi, t), p.on_accent) } else { (p.surface_hi, p.faint) };
    let rect = if enabled && resp.is_pointer_button_down_on() { rect.shrink(1.0) } else { rect };
    if enabled {
        // Soft glow under the button.
        ui.painter().rect_filled(rect.translate(Vec2::new(0.0, 3.0)).expand(1.0), CornerRadius::same(12), p.accent.gamma_multiply(0.18 + 0.12 * t));
    }
    ui.painter().rect_filled(rect, CornerRadius::same(12), fill);
    if !enabled {
        ui.painter().rect_stroke(rect, CornerRadius::same(12), Stroke::new(1.0_f32, p.stroke), StrokeKind::Inside);
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(17.0), fg);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// A painted folder in a tinted tile (the emoji fonts' folder is tiny).
fn folder_icon(ui: &mut egui::Ui, p: Pal) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), p.accent.gamma_multiply(0.13));
    let body = Rect::from_center_size(rect.center() + Vec2::new(0.0, 1.5), Vec2::new(20.0, 14.0));
    let tab = Rect::from_min_size(body.min - Vec2::new(0.0, 3.0), Vec2::new(9.0, 5.0));
    painter.rect_filled(tab, CornerRadius::same(2), p.accent);
    painter.rect_filled(body, CornerRadius::same(3), p.accent);
    painter.line_segment(
        [body.left_top() + Vec2::new(0.0, 4.0), body.right_top() + Vec2::new(0.0, 4.0)],
        Stroke::new(1.2_f32, p.accent.gamma_multiply(0.13).to_opaque()),
    );
}

/// The app mark: a rounded tile with a camera lens.
fn logo(ui: &mut egui::Ui, p: Pal) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), p.accent);
    let c = rect.center();
    painter.circle_stroke(c, 10.5, Stroke::new(2.4_f32, p.on_accent));
    painter.circle_filled(c, 4.5, p.on_accent);
    painter.circle_filled(c + Vec2::new(-1.6, -1.6), 1.5, p.accent);
    // Recording dot.
    painter.circle_filled(rect.right_top() + Vec2::new(-7.0, 7.0), 3.0, p.on_accent);
}

/// Status badge for a file card.
fn status_badge(ui: &mut egui::Ui, p: Pal, s: &Status) {
    match s {
        Status::Idle => {}
        Status::Queued => {
            pill(ui, "Queued", p.faint);
        }
        Status::Running { done, total } => {
            pill(ui, &format!("Converting {done}/{total}"), p.accent);
        }
        Status::Done { .. } => {
            pill(ui, "Done", p.ok);
        }
        Status::Failed(_) => {
            pill(ui, "Failed", p.err);
        }
    }
}

/// Round icon at the left of a file card.
fn status_icon(ui: &mut egui::Ui, p: Pal, s: &Status) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
    let (tint, glyph) = match s {
        Status::Idle | Status::Queued => (p.dim, Some("🎞")),
        Status::Running { .. } => (p.accent, None),
        Status::Done { .. } => (p.ok, Some("✔")),
        Status::Failed(_) => (p.err, Some("✖")),
    };
    ui.painter().rect_filled(rect, CornerRadius::same(10), tint.gamma_multiply(0.13));
    match glyph {
        Some(g) => {
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, g, FontId::proportional(18.0), tint);
        }
        None => {
            egui::Spinner::new().size(18.0).color(tint).paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(18.0)));
        }
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.debug_screenshot(ctx);
        // Hidden, for screenshots of the busy state: DVRAVC_GUI_AUTOCONVERT=1.
        if self.run.is_none() && !self.items.is_empty() && std::env::var_os("DVRAVC_GUI_AUTOCONVERT").is_some() && self.ffmpeg {
            self.start();
        }
        self.poll();
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add(dropped);

        self.header(ctx);
        self.log_panel(ctx);
        egui::CentralPanel::default().frame(egui::Frame::new().fill(ctx.style().visuals.panel_fill)).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                // A centred column with a comfortable reading width.
                let width = (ui.available_width() - 48.0).min(760.0);
                let pad = ((ui.available_width() - width) / 2.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        ui.add_space(18.0);
                        self.body(ui, hovering);
                        ui.add_space(24.0);
                    });
                });
            });
        });

        if hovering {
            // Whole-window hint while files are dragged over it.
            let p = if ctx.style().visuals.dark_mode { theme::DARK } else { theme::LIGHT };
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop")));
            let r = ctx.screen_rect().shrink(10.0);
            painter.rect_filled(r, CornerRadius::same(16), p.bg.gamma_multiply(0.75));
            painter.add(Shape::dashed_line(&rounded_path(r, 16.0), Stroke::new(2.0_f32, p.accent), 10.0, 7.0));
            painter.text(r.center(), Align2::CENTER_CENTER, "Drop to add recordings", FontId::proportional(24.0), p.accent);
        }
    }
}

impl App {
    /// Hidden, for README screenshots: `DVRAVC_GUI_SCREENSHOT=out.ppm` saves the
    /// window's own framebuffer (no compositor effects) after ~1 s, then exits.
    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = std::env::var_os("DVRAVC_GUI_SCREENSHOT") else { return };
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let mut ppm = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
            ppm.extend(img.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]));
            if let Err(e) = std::fs::write(&path, ppm) {
                eprintln!("screenshot: {e}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let t = ctx.input(|i| i.time);
        if t > 1.0 && !self.shot_requested {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn header(&mut self, ctx: &egui::Context) {
        let p = if ctx.style().visuals.dark_mode { theme::DARK } else { theme::LIGHT };
        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::new().fill(p.surface).inner_margin(Margin::symmetric(20, 12)).stroke(Stroke::new(1.0_f32, p.stroke)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    logo(ui, p);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(RichText::new("dvravc").size(20.0).color(p.text).strong());
                        ui.label(RichText::new("DVR .avc recordings to MP4").size(12.5).color(p.dim));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let pref = ctx.options(|o| o.theme_preference);
                        let (icon, next, tip) = match pref {
                            ThemePreference::System => ("🌓", ThemePreference::Dark, "Theme: system"),
                            ThemePreference::Dark => ("🌙", ThemePreference::Light, "Theme: dark"),
                            ThemePreference::Light => ("☀", ThemePreference::System, "Theme: light"),
                        };
                        if ui.add(egui::Button::new(RichText::new(icon).size(16.0)).frame(false)).on_hover_text(tip).clicked() {
                            ctx.set_theme(next);
                        }
                        ui.add_space(4.0);
                        if self.ffmpeg {
                            pill(ui, "ffmpeg ready", p.ok).on_hover_text("ffmpeg was found on PATH");
                        } else if pill(ui, "ffmpeg missing", p.err)
                            .on_hover_text(format!("{}\n\nClick to check again.", convert::FFMPEG_HINT))
                            .interact(Sense::click())
                            .clicked()
                        {
                            self.ffmpeg = convert::ffmpeg_available();
                        }
                    });
                });
            });
    }

    fn log_panel(&mut self, ctx: &egui::Context) {
        let p = if ctx.style().visuals.dark_mode { theme::DARK } else { theme::LIGHT };
        egui::TopBottomPanel::bottom("log")
            .resizable(self.log_open)
            .min_height(if self.log_open { 140.0 } else { 0.0 })
            .default_height(200.0)
            .frame(egui::Frame::new().fill(p.surface).inner_margin(Margin::symmetric(20, 10)).stroke(Stroke::new(1.0_f32, p.stroke)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(150.0, 22.0), Sense::click());
                    // Chevron + title, all clickable.
                    let c = Pos2::new(rect.left() + 6.0, rect.center().y);
                    let pts = if self.log_open {
                        vec![c + Vec2::new(-4.5, -2.5), c + Vec2::new(4.5, -2.5), c + Vec2::new(0.0, 3.0)]
                    } else {
                        vec![c + Vec2::new(-2.5, -4.5), c + Vec2::new(3.0, 0.0), c + Vec2::new(-2.5, 4.5)]
                    };
                    let col = if resp.hovered() { p.text } else { p.dim };
                    ui.painter().add(Shape::convex_polygon(pts, col, Stroke::NONE));
                    let label = format!("Log  ·  {} line{}", self.log.len(), if self.log.len() == 1 { "" } else { "s" });
                    ui.painter().text(Pos2::new(rect.left() + 18.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), col);
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.log_open = !self.log_open;
                    }
                    if self.log_open {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add_enabled(!self.log.is_empty(), egui::Button::new(RichText::new("Clear").size(12.5)).frame(false)).clicked() {
                                self.log.clear();
                            }
                            if ui.add_enabled(!self.log.is_empty(), egui::Button::new(RichText::new("Copy").size(12.5)).frame(false)).clicked() {
                                ctx.copy_text(self.log.join("\n"));
                            }
                        });
                    }
                });
                if self.log_open {
                    ui.add_space(6.0);
                    egui::Frame::new().fill(p.well).corner_radius(CornerRadius::same(8)).inner_margin(Margin::same(10)).show(ui, |ui| {
                        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
                            if self.log.is_empty() {
                                ui.label(RichText::new("Nothing yet.").monospace().color(p.faint));
                            }
                            for line in &self.log {
                                let color = if line.starts_with("ERROR") || line.contains(" failed") {
                                    p.err
                                } else if line.starts_with("Done") {
                                    p.ok
                                } else if line.starts_with("  ") {
                                    p.dim
                                } else {
                                    p.text
                                };
                                ui.label(RichText::new(line).monospace().color(color));
                            }
                        });
                    });
                }
            });
    }

    fn body(&mut self, ui: &mut egui::Ui, hovering: bool) {
        let p = theme::pal(ui);
        let busy = self.busy();

        if !self.ffmpeg {
            egui::Frame::new()
                .fill(p.err.gamma_multiply(0.08))
                .stroke(Stroke::new(1.0_f32, p.err.gamma_multiply(0.5)))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("⚠").size(18.0).color(p.err));
                        ui.label(RichText::new("ffmpeg is required to convert").color(p.text).size(15.5));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Check again").clicked() {
                                self.ffmpeg = convert::ffmpeg_available();
                            }
                        });
                    });
                    ui.add_space(4.0);
                    for line in convert::FFMPEG_HINT.lines().skip(1) {
                        ui.label(RichText::new(line.trim()).monospace().color(p.dim));
                    }
                });
            ui.add_space(14.0);
        }

        // Drop zone.
        let compact = !self.items.is_empty();
        let height = if compact { 74.0 } else { 190.0 };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
        let hot = hovering || resp.hovered();
        let t = ui.ctx().animate_bool_responsive(resp.id, hot);
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(14), lerp_color(p.surface, p.accent.gamma_multiply(0.10), t));
        let dash = lerp_color(p.stroke_hi, p.accent, t);
        painter.add(Shape::dashed_line(&rounded_path(rect.shrink(1.0), 14.0), Stroke::new(1.5_f32, dash), 8.0, 6.0));
        let title = if self.can_drop { "Drop .avc files or click to browse" } else { "Click to browse for .avc files" };
        let sub = if self.can_drop { "Folders work too: every .avc inside is added" } else { "Drag and drop isn't available on Wayland" };
        if compact {
            let icon_c = Pos2::new(rect.left() + 38.0, rect.center().y);
            painter.circle_filled(icon_c, 18.0, p.accent.gamma_multiply(0.14 + 0.1 * t));
            painter.text(icon_c, Align2::CENTER_CENTER, "⬇", FontId::proportional(16.0), p.accent);
            let sub = if self.can_drop { "Drop files or folders here, or click to browse" } else { "Click to browse" };
            painter.text(Pos2::new(rect.left() + 68.0, rect.center().y - 9.0), Align2::LEFT_CENTER, "Add more recordings", FontId::proportional(15.0), p.text);
            painter.text(Pos2::new(rect.left() + 68.0, rect.center().y + 11.0), Align2::LEFT_CENTER, sub, FontId::proportional(12.5), p.dim);
        } else {
            let c = rect.center() - Vec2::new(0.0, 26.0);
            painter.circle_filled(c, 28.0 + 2.0 * t, p.accent.gamma_multiply(0.14 + 0.1 * t));
            painter.text(c, Align2::CENTER_CENTER, "⬇", FontId::proportional(24.0), p.accent);
            painter.text(rect.center() + Vec2::new(0.0, 22.0), Align2::CENTER_CENTER, title, FontId::proportional(17.5), p.text);
            painter.text(rect.center() + Vec2::new(0.0, 46.0), Align2::CENTER_CENTER, sub, FontId::proportional(13.0), p.dim);
        }
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
            && let Some(f) = rfd::FileDialog::new().add_filter("DVR recording", &["avc", "AVC"]).pick_files()
        {
            self.add(f);
        }

        // File list.
        if !self.items.is_empty() {
            ui.add_space(22.0);
            ui.horizontal(|ui| {
                section_label(ui, p, &format!("Recordings  ·  {}", self.items.len()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!busy, egui::Button::new(RichText::new("Clear all").size(12.5).color(p.dim)).frame(false)).clicked() {
                        self.items.clear();
                        self.run = None;
                    }
                    if ui.add_enabled(!busy, egui::Button::new(RichText::new("Add folder…").size(12.5).color(p.dim)).frame(false)).clicked()
                        && let Some(d) = rfd::FileDialog::new().pick_folder()
                    {
                        self.add([d]);
                    }
                });
            });
            ui.add_space(2.0);
            let mut remove = None;
            for (idx, item) in self.items.iter().enumerate() {
                if Self::file_card(ui, p, item, busy) {
                    remove = Some(idx);
                }
                ui.add_space(8.0);
            }
            if let Some(i) = remove {
                self.items.remove(i);
                self.run = None;
            }
        }

        // Output settings.
        ui.add_space(16.0);
        section_label(ui, p, "Output");
        ui.add_space(2.0);
        card(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let out = self.out_dir();
            ui.horizontal(|ui| {
                folder_icon(ui, p);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let exists = out.as_ref().is_some_and(|d| d.is_dir());
                    if ui.add_enabled(exists, egui::Button::new("Open")).on_hover_text("Show in the file manager").clicked()
                        && let Some(d) = &out
                        && let Err(e) = open_in_file_manager(d)
                    {
                        self.log.push(format!("Couldn't open {}: {e}", d.display()));
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Browse…")).clicked() {
                        let mut dlg = rfd::FileDialog::new();
                        if let Some(d) = out.as_deref().filter(|d| d.is_dir()).or(out.as_deref().and_then(Path::parent)) {
                            dlg = dlg.set_directory(d);
                        }
                        if let Some(d) = dlg.pick_folder() {
                            self.out_dir = Some(d);
                        }
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            match &out {
                                Some(d) => {
                                    ui.add(egui::Label::new(RichText::new(d.display().to_string()).monospace().color(p.text)).truncate())
                                        .on_hover_text(d.display().to_string());
                                    let hint = if self.out_dir.is_some() { "One subfolder per recording" } else { "Next to the first recording · one subfolder each" };
                                    ui.label(RichText::new(hint).size(12.0).color(p.faint));
                                }
                                None => {
                                    ui.label(RichText::new("Next to your recordings").color(p.dim));
                                    ui.label(RichText::new("or pick a folder").size(12.0).color(p.faint));
                                }
                            }
                        });
                    });
                });
            });
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            let row = |ui: &mut egui::Ui, on: &mut bool, title: &str, desc: &str| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(RichText::new(title).color(p.text));
                        ui.label(RichText::new(desc).size(12.5).color(p.dim));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| toggle(ui, p, on));
                });
            };
            ui.add_enabled_ui(!busy, |ui| {
                row(ui, &mut self.opts.channels, "One MP4 per camera", "CH1_NAME.mp4 … at native resolution, with audio");
                ui.add_space(8.0);
                row(ui, &mut self.opts.mosaic, "2×2 mosaic", "All cameras in one video, time-aligned and labelled");
            });
        });

        // Action area.
        ui.add_space(20.0);
        if busy {
            let frac = self.progress();
            card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(16.0).color(p.accent));
                    let n = self.run.as_ref().map_or(0, |r| r.jobs.len());
                    ui.label(RichText::new(format!("Converting {n} recording{}", if n == 1 { "" } else { "s" })).color(p.text));
                    ui.label(RichText::new(format!("{:.0}%", frac * 100.0)).color(p.accent).monospace());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Cancel").clicked()
                            && let Some(b) = self.run.as_ref().and_then(|r| r.batch.as_ref())
                        {
                            b.cancel.cancel();
                            self.log.push("Cancelling…".into());
                        }
                    });
                });
                ui.add_space(8.0);
                bar(ui, p, frac, 8.0);
            });
        } else {
            if let Some(run) = &self.run
                && run.batch.is_none()
                && run.ok + run.failed > 0
            {
                
                let (color, glyph, text) = if run.failed == 0 {
                    (p.ok, "✔", format!("{} recording{} converted", run.ok, if run.ok == 1 { "" } else { "s" }))
                } else {
                    (p.err, "⚠", format!("{} converted, {} failed (see the log)", run.ok, run.failed))
                };
                let out_dir = run.out_dir.clone();
                let banner = egui::Frame::new()
                    .fill(color.gamma_multiply(0.08))
                    .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.45)))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(glyph).size(18.0).color(color));
                            ui.label(RichText::new(text).color(p.text));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("Open output folder").clicked()
                                    && let Err(e) = open_in_file_manager(&out_dir)
                                {
                                    self.log.push(format!("Couldn't open {}: {e}", out_dir.display()));
                                }
                            });
                        });
                    });
                if std::mem::take(&mut self.scroll_to_result) {
                    banner.response.scroll_to_me(Some(Align::Center));
                }
                ui.add_space(14.0);
            }
            let n = self.items.len();
            let reason = if !self.ffmpeg {
                Some("Install ffmpeg to convert")
            } else if n == 0 {
                Some("Add a recording to start")
            } else if !self.opts.channels && !self.opts.mosaic {
                Some("Turn on at least one output")
            } else {
                None
            };
            let label = if n == 0 { "Convert".to_string() } else { format!("▶   Convert {n} recording{}", if n == 1 { "" } else { "s" }) };
            if big_button(ui, p, &label, reason.is_none()).clicked() {
                self.start();
            }
            if let Some(r) = reason {
                ui.add_space(4.0);
                ui.vertical_centered(|ui| ui.label(RichText::new(r).size(12.5).color(p.faint)));
            }
        }
    }

    /// One recording. Returns true if its remove button was clicked.
    fn file_card(ui: &mut egui::Ui, p: Pal, item: &Item, busy: bool) -> bool {
        let mut remove = false;
        let stroke = match item.status {
            Status::Running { .. } => p.accent.gamma_multiply(0.6),
            Status::Failed(_) => p.err.gamma_multiply(0.5),
            _ => p.stroke,
        };
        card(p).stroke(Stroke::new(1.0_f32, stroke)).inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                status_icon(ui, p, &item.status);
                ui.add_space(2.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !busy
                        && ui
                            .add(egui::Button::new(RichText::new("✖").size(13.0).color(p.faint)).frame(false))
                            .on_hover_text("Remove")
                            .clicked()
                    {
                        remove = true;
                    }
                    if let Status::Done { dir } = &item.status
                        && ui
                            .add(egui::Button::new(RichText::new("Show").size(12.5).color(p.accent)).frame(false))
                            .on_hover_text(format!("Open {}", dir.display()))
                            .clicked()
                    {
                        let _ = open_in_file_manager(dir);
                    }
                    status_badge(ui, p, &item.status);
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            ui.add(egui::Label::new(RichText::new(item.name()).size(15.5).color(p.text)).truncate())
                                .on_hover_text(item.path.display().to_string());
                            let mut meta: Vec<String> = Vec::new();
                            match &item.info {
                                Info::Loading => meta.push("Reading…".into()),
                                Info::Err(e) => meta.push(e.clone()),
                                Info::Ok(s) => {
                                    if let Some(t) = s.time {
                                        meta.push(format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second));
                                    }
                                    meta.push(human_duration(s.duration));
                                    meta.push(format!("{} camera{}", s.channels.len(), if s.channels.len() == 1 { "" } else { "s" }));
                                    if s.audio {
                                        meta.push("audio".into());
                                    }
                                }
                            }
                            if let Some(n) = item.size {
                                meta.push(human_size(n));
                            }
                            let meta_color = if matches!(item.info, Info::Err(_)) { p.err } else { p.dim };
                            ui.add(egui::Label::new(RichText::new(meta.join("  ·  ")).size(12.5).color(meta_color)).truncate());
                        });
                    });
                });
            });
            if let Info::Ok(s) = &item.info
                && !s.channels.is_empty()
            {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(52.0);
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    for (ch, title) in &s.channels {
                        egui::Frame::new()
                            .fill(p.surface_hi)
                            .stroke(Stroke::new(1.0_f32, p.stroke))
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(Margin::symmetric(7, 2))
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                ui.label(RichText::new(format!("CH{ch}")).size(11.5).color(p.accent).monospace());
                                if !title.is_empty() {
                                    ui.label(RichText::new(title).size(12.0).color(p.dim));
                                }
                            });
                    }
                });
            }
            match &item.status {
                Status::Running { done, total } => {
                    ui.add_space(8.0);
                    bar(ui, p, *done as f32 / *total as f32, 4.0);
                }
                // A parse error is already shown in the meta line.
                Status::Failed(e) if !matches!(item.info, Info::Err(_)) => {
                    ui.add_space(6.0);
                    let first = e.lines().next().unwrap_or_default();
                    // "encoding …/name.avc: reason" -> "reason"
                    let path = item.path.display().to_string();
                    let first = first.rfind(&path).map_or(first, |i| first[i + path.len()..].trim_start_matches(':').trim());
                    ui.horizontal(|ui| {
                        ui.add_space(52.0);
                        ui.add(egui::Label::new(RichText::new(first).size(12.5).color(p.err)).truncate()).on_hover_text(e);
                    });
                }
                _ => {}
            }
        });
        remove
    }
}
