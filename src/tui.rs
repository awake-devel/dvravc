//! Terminal UI (ratatui).

use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use anyhow::Result;
use dvravc::convert::{self, Batch, Msg, Options};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

const SPINNER: &[char] = &['|', '/', '-', '\\'];

struct App {
    dir: PathBuf,
    files: Vec<PathBuf>,
    selected: Vec<bool>,
    list: ListState,
    opts: Options,
    out_dir: PathBuf,
    /// `Some(buffer)` while editing the output dir.
    editing: Option<String>,
    log: Vec<String>,
    worker: Option<Batch>,
    tick: usize,
    ffmpeg: bool,
}

pub fn run(dir: PathBuf) -> Result<()> {
    let mut app = App {
        out_dir: dir.join("output"),
        dir,
        files: vec![],
        selected: vec![],
        list: ListState::default(),
        opts: Options::default(),
        editing: None,
        log: vec![],
        worker: None,
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
            // Raw mode swallows SIGINT, so handle Ctrl+C as quit.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(());
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
                KeyCode::Char('c') => self.opts.channels = !self.opts.channels,
                KeyCode::Char('m') => self.opts.mosaic = !self.opts.mosaic,
                KeyCode::Char('o') => self.editing = Some(self.out_dir.display().to_string()),
                KeyCode::Char('r') if !self.busy() => self.rescan(),
                KeyCode::Enter if !self.busy() => self.start(),
                _ => {}
            }
        }
    }

    fn start(&mut self) {
        let mut files: Vec<PathBuf> = self
            .files
            .iter()
            .zip(&self.selected)
            .filter(|(_, s)| **s)
            .map(|(f, _)| f.clone())
            .collect();
        if files.is_empty() {
            files.extend(self.list.selected().and_then(|i| self.files.get(i)).cloned());
        }
        if files.is_empty() {
            return;
        }
        if !self.opts.channels && !self.opts.mosaic {
            self.log.push("Nothing to do: enable channels (c) or mosaic (m).".into());
            return;
        }
        self.log.push(format!("Converting {} file(s) to {}", files.len(), self.out_dir.display()));
        self.worker = Some(convert::spawn_batch(files, self.out_dir.clone(), self.opts));
    }

    fn poll_worker(&mut self) {
        let Some(b) = &self.worker else { return };
        loop {
            match b.rx.try_recv() {
                Ok(Msg::Log(s)) => self.log.extend(s.lines().map(String::from)),
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

    fn draw(&mut self, f: &mut Frame) {
        let [top, opts, log, help] = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Percentage(40),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let items: Vec<ListItem> = self
            .files
            .iter()
            .zip(&self.selected)
            .map(|(p, &s)| {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                ListItem::new(format!("[{}] {name}", if s { 'x' } else { ' ' }))
            })
            .collect();
        let title = format!(" .avc files in {} ({}) ", self.dir.display(), self.files.len());
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        f.render_stateful_widget(list, top, &mut self.list);

        let check = |b: bool| if b { "[x]" } else { "[ ]" };
        let out = match &self.editing {
            Some(buf) => format!("{buf}_  (Enter: ok, Esc: cancel)"),
            None => self.out_dir.display().to_string(),
        };
        let status = if self.busy() {
            format!("  {} converting...", SPINNER[self.tick % SPINNER.len()]).yellow()
        } else {
            "".into()
        };
        let line = Line::from(vec![
            format!("{} channels  {} mosaic   output: {out}", check(self.opts.channels), check(self.opts.mosaic)).into(),
            status,
        ]);
        f.render_widget(Paragraph::new(line).block(Block::bordered().title(" Options ")), opts);

        // Show the tail of the log that fits, counting wrapped rows.
        let (h, w) = (log.height.saturating_sub(2) as usize, log.width.saturating_sub(2).max(1) as usize);
        let mut rows = 0;
        let first = self
            .log
            .iter()
            .rposition(|s| {
                rows += s.chars().count().div_ceil(w).max(1);
                rows > h
            })
            .map_or(0, |i| i + 1);
        let lines: Vec<Line> = self.log[first..].iter().map(|s| Line::from(s.as_str())).collect();
        let log_title = if self.ffmpeg { " Log " } else { " Log (ffmpeg missing!) " };
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered().title(log_title)),
            log,
        );

        f.render_widget(
            Paragraph::new(
                "↑↓/jk move  space select  a all  c channels  m mosaic  o output dir  Enter convert  r rescan  q quit",
            )
            .style(Style::new().fg(Color::DarkGray)),
            help,
        );
    }
}
