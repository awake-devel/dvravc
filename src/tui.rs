//! Terminal UI (ratatui).

use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use anyhow::Result;
use dvravc::convert::{self, Batch, Msg, Options};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Gauge, List, ListItem, ListState, Padding, Paragraph};
use ratatui::{DefaultTerminal, Frame};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Rgb(45, 212, 191);
const DIM: Color = Color::Rgb(120, 130, 142);
const OK: Color = Color::Rgb(52, 211, 153);
const ERR: Color = Color::Rgb(248, 113, 113);
const SEL_BG: Color = Color::Rgb(19, 48, 46);

#[derive(Clone)]
enum Status {
    Idle,
    Queued,
    Running { done: usize, total: usize },
    Done,
    Failed,
}

struct App {
    dir: PathBuf,
    files: Vec<PathBuf>,
    selected: Vec<bool>,
    status: Vec<Status>,
    list: ListState,
    opts: Options,
    out_dir: PathBuf,
    /// `Some(buffer)` while editing the output dir.
    editing: Option<String>,
    log: Vec<String>,
    worker: Option<Batch>,
    /// Batch index -> index in `files`.
    jobs: Vec<usize>,
    tick: usize,
    ffmpeg: bool,
}

pub fn run(dir: PathBuf) -> Result<()> {
    let mut app = App {
        out_dir: dir.join("output"),
        dir,
        files: vec![],
        selected: vec![],
        status: vec![],
        list: ListState::default(),
        opts: Options::default(),
        editing: None,
        log: vec![],
        worker: None,
        jobs: vec![],
        tick: 0,
        ffmpeg: convert::ffmpeg_available(),
    };
    app.rescan();
    if !app.ffmpeg {
        app.log.extend(convert::FFMPEG_HINT.lines().map(String::from));
    }
    // ratatui::init also installs a panic hook that restores the terminal.
    let mut terminal = ratatui::init();
    let res = app.main_loop(&mut terminal);
    app.stop_worker();
    ratatui::restore();
    res
}

impl App {
    fn rescan(&mut self) {
        match convert::scan_dir(&self.dir) {
            Ok(f) => self.files = f,
            Err(e) => self.log.push(format!("{e:#}")),
        }
        self.selected = vec![false; self.files.len()];
        self.status = vec![Status::Idle; self.files.len()];
        self.list.select((!self.files.is_empty()).then_some(0));
    }

    fn busy(&self) -> bool {
        self.worker.is_some()
    }

    fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.poll_worker();
            terminal.draw(|f| self.draw(f))?;
            if !event::poll(Duration::from_millis(100))? {
                self.tick += 1;
                continue;
            }
            // Windows also reports key releases; only act on presses.
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // Raw mode swallows SIGINT, so handle Ctrl+C as quit.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(());
            }
            if let Some(buf) = &mut self.editing {
                match key.code {
                    KeyCode::Enter => {
                        self.out_dir = PathBuf::from(buf.trim());
                        self.editing = None;
                    }
                    KeyCode::Esc => self.editing = None,
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => self.list.select_next(),
                KeyCode::Up | KeyCode::Char('k') => self.list.select_previous(),
                KeyCode::Char(' ') => {
                    if let Some(s) = self.list.selected().and_then(|i| self.selected.get_mut(i)) {
                        *s = !*s;
                    }
                }
                KeyCode::Char('a') => {
                    let all = self.selected.iter().all(|&s| s);
                    self.selected.fill(!all);
                }
                KeyCode::Char('c') if !self.busy() => self.opts.channels = !self.opts.channels,
                KeyCode::Char('m') if !self.busy() => self.opts.mosaic = !self.opts.mosaic,
                KeyCode::Char('o') if !self.busy() => self.editing = Some(self.out_dir.display().to_string()),
                KeyCode::Char('r') if !self.busy() => self.rescan(),
                KeyCode::Char('x') if self.busy() => {
                    if let Some(b) = &self.worker {
                        b.cancel.cancel();
                        self.log.push("Cancelling…".into());
                    }
                }
                KeyCode::Enter if !self.busy() => self.start(),
                _ => {}
            }
        }
    }

    fn start(&mut self) {
        let mut jobs: Vec<usize> = (0..self.files.len()).filter(|&i| self.selected[i]).collect();
        if jobs.is_empty() {
            jobs.extend(self.list.selected().filter(|&i| i < self.files.len()));
        }
        if jobs.is_empty() {
            return;
        }
        if !self.opts.channels && !self.opts.mosaic {
            self.log.push("Nothing to do: enable channels (c) or mosaic (m).".into());
            return;
        }
        let files: Vec<PathBuf> = jobs.iter().map(|&i| self.files[i].clone()).collect();
        self.status.fill(Status::Idle);
        for &i in &jobs {
            self.status[i] = Status::Queued;
        }
        self.log.push(format!("Converting {} file(s) to {}", files.len(), self.out_dir.display()));
        self.worker = Some(convert::spawn_batch(files, self.out_dir.clone(), self.opts));
        self.jobs = jobs;
    }

    fn poll_worker(&mut self) {
        let Some(b) = &self.worker else { return };
        loop {
            let msg = match b.rx.try_recv() {
                Ok(m) => m,
                Err(TryRecvError::Empty) => return,
                // The worker died without saying so: don't stay "busy" forever.
                Err(TryRecvError::Disconnected) => {
                    self.log.push("Conversion stopped unexpectedly".into());
                    for s in &mut self.status {
                        if matches!(s, Status::Queued | Status::Running { .. }) {
                            *s = Status::Failed;
                        }
                    }
                    self.worker = None;
                    return;
                }
            };
            let slot = |index: usize| self.jobs.get(index).copied();
            match msg {
                Msg::Log(s) => self.log.extend(s.lines().map(String::from)),
                Msg::Started { index } => {
                    if let Some(s) = slot(index).and_then(|i| self.status.get_mut(i)) {
                        *s = Status::Running { done: 0, total: 1 };
                    }
                }
                Msg::Step { index, done, total } => {
                    if let Some(s) = slot(index).and_then(|i| self.status.get_mut(i)) {
                        *s = Status::Running { done, total: total.max(1) };
                    }
                }
                Msg::FileDone { index, error, .. } => {
                    if let Some(s) = slot(index).and_then(|i| self.status.get_mut(i)) {
                        *s = if error.is_none() { Status::Done } else { Status::Failed };
                    }
                }
                Msg::Finished { ok, failed } => {
                    self.log.push(format!("Done: {ok} ok, {failed} failed"));
                    self.worker = None;
                    return;
                }
            }
        }
    }

    /// Stop a running batch (kills ffmpeg, removes the partial file) and wait for it.
    fn stop_worker(&mut self) {
        if let Some(b) = self.worker.take() {
            b.cancel.cancel();
            while let Ok(msg) = b.rx.recv_timeout(Duration::from_secs(5)) {
                if matches!(msg, Msg::Finished { .. }) {
                    break;
                }
            }
        }
    }

    /// `(finished fraction, label)` of the current or last batch.
    fn progress(&self) -> Option<(f64, String)> {
        if self.jobs.is_empty() {
            return None;
        }
        let mut sum = 0.0;
        let mut finished = 0;
        for &i in &self.jobs {
            match self.status.get(i) {
                Some(Status::Running { done, total }) => sum += *done as f64 / *total as f64,
                Some(Status::Done | Status::Failed) => {
                    sum += 1.0;
                    finished += 1;
                }
                _ => {}
            }
        }
        let frac = sum / self.jobs.len() as f64;
        Some((frac, format!("{finished}/{} files · {:.0}%", self.jobs.len(), frac * 100.0)))
    }

    fn panel(title: &str) -> Block<'_> {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::Rgb(55, 65, 78)))
            .title(Line::from(format!(" {title} ")).fg(ACCENT).bold())
            .padding(Padding::horizontal(1))
    }

    fn draw(&mut self, f: &mut Frame) {
        let [title, top, opts, gauge, log, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(4),
            Constraint::Length(if self.jobs.is_empty() { 0 } else { 3 }),
            Constraint::Percentage(35),
            Constraint::Length(1),
        ])
        .areas(f.area());

        self.draw_title(f, title);

        let spin = SPINNER[self.tick % SPINNER.len()];
        let items: Vec<ListItem> = self
            .files
            .iter()
            .zip(&self.selected)
            .zip(&self.status)
            .map(|((p, &sel), st)| {
                let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let check = if sel { Span::from("◉ ").fg(ACCENT) } else { Span::from("○ ").fg(DIM) };
                let status = match st {
                    Status::Idle => Span::raw(""),
                    Status::Queued => Span::from("  · queued").fg(DIM),
                    Status::Running { done, total } => Span::from(format!("  {spin} {done}/{total}")).fg(ACCENT),
                    Status::Done => Span::from("  ✔ done").fg(OK),
                    Status::Failed => Span::from("  ✖ failed").fg(ERR),
                };
                ListItem::new(Line::from(vec![check, Span::raw(name), status]))
            })
            .collect();
        let title = format!("Recordings ({}) in {}", self.files.len(), self.dir.display());
        let list = List::new(items)
            .block(Self::panel(&title))
            .highlight_style(Style::new().bg(SEL_BG).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌ ");
        if self.files.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from("No .avc files here. Run `dvravc tui DIR` or press r to rescan.").fg(DIM))
                    .block(Self::panel(&title)),
                top,
            );
        } else {
            f.render_stateful_widget(list, top, &mut self.list);
        }

        let toggle = |on: bool, key: &'static str, label: &'static str| {
            vec![
                Span::from(if on { "● " } else { "○ " }).fg(if on { ACCENT } else { DIM }),
                Span::from(label).fg(if on { Color::Reset } else { DIM }),
                Span::from(format!(" [{key}]   ")).fg(DIM),
            ]
        };
        let mut line1 = toggle(self.opts.channels, "c", "Per-channel MP4s");
        line1.extend(toggle(self.opts.mosaic, "m", "2×2 mosaic"));
        let out = match &self.editing {
            Some(buf) => Line::from(vec![
                Span::from("Output  ").fg(DIM),
                Span::from(format!("{buf}▏")).fg(ACCENT),
                Span::from("  Enter ok · Esc cancel").fg(DIM),
            ]),
            None => Line::from(vec![
                Span::from("Output  ").fg(DIM),
                Span::raw(self.out_dir.display().to_string()),
                Span::from("  [o]").fg(DIM),
            ]),
        };
        f.render_widget(Paragraph::new(vec![Line::from(line1), out]).block(Self::panel("Options")), opts);

        if let Some((frac, label)) = self.progress() {
            let color = if self.status.iter().any(|s| matches!(s, Status::Failed)) && !self.busy() { ERR } else { ACCENT };
            f.render_widget(
                Gauge::default()
                    .block(Self::panel(if self.busy() { "Converting" } else { "Last run" }))
                    .gauge_style(Style::new().fg(color).bg(Color::Rgb(30, 36, 44)))
                    .ratio(frac.clamp(0.0, 1.0))
                    .label(Span::from(label).fg(Color::White).bold())
                    .use_unicode(true),
                gauge,
            );
        }

        // Show the tail of the log that fits, counting wrapped rows.
        let (h, w) = (log.height.saturating_sub(2) as usize, log.width.saturating_sub(4).max(1) as usize);
        let mut rows = 0;
        let first = self
            .log
            .iter()
            .rposition(|s| {
                rows += s.chars().count().div_ceil(w).max(1);
                rows > h
            })
            .map_or(0, |i| i + 1);
        let lines: Vec<Line> = self.log[first..]
            .iter()
            .map(|s| {
                let style = if s.starts_with("ERROR") || s.contains(" failed") {
                    Style::new().fg(ERR)
                } else if s.starts_with("Done") {
                    Style::new().fg(OK)
                } else if s.starts_with("  ") {
                    Style::new().fg(DIM)
                } else {
                    Style::new()
                };
                Line::styled(s.as_str(), style)
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }).block(Self::panel("Log")),
            log,
        );

        let keys: &[(&str, &str)] = if self.busy() {
            &[("↑↓", "move"), ("x", "cancel"), ("q", "quit")]
        } else {
            &[
                ("↑↓/jk", "move"),
                ("space", "select"),
                ("a", "all"),
                ("c/m", "outputs"),
                ("o", "output dir"),
                ("enter", "convert"),
                ("r", "rescan"),
                ("q", "quit"),
            ]
        };
        let spans: Vec<Span> = keys
            .iter()
            .flat_map(|(k, d)| [Span::from(format!(" {k} ")).fg(ACCENT).bold(), Span::from(format!("{d}  ")).fg(DIM)])
            .collect();
        f.render_widget(Paragraph::new(Line::from(spans)), help);
    }

    fn draw_title(&self, f: &mut Frame, area: Rect) {
        let status = if self.ffmpeg {
            Span::from(" ● ffmpeg ready ").fg(OK)
        } else {
            Span::from(" ● ffmpeg missing ").fg(ERR)
        };
        let left = Line::from(vec![
            Span::from(" ◉ dvravc ").fg(Color::Black).bg(ACCENT).bold(),
            Span::from("  DVR .avc recordings to MP4").fg(DIM),
        ]);
        let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(status.width() as u16)]).areas(area);
        f.render_widget(Paragraph::new(left), l);
        f.render_widget(Paragraph::new(Line::from(status)), r);
    }
}
