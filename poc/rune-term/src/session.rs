//! One program in a pseudo-terminal, its screen kept by Alacritty's terminal engine.
//!
//! Threads, as in Switchyard's backend (src-tauri/src/pty.rs):
//! - a reader parses output into the terminal as it arrives, in chunks, releasing the lock
//!   between them so a frame never waits long for a burst of output;
//! - a writer sends keystrokes, so a program that stops reading never blocks the UI;
//! - the UI thread only reads the screen (to draw it) and resizes.
//!
//! The same bytes also go through a second, tiny parser that picks out Claude Code's own
//! status reports (OSC 7501), which the terminal engine ignores.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use alacritty_terminal::vte::{Params, Parser, Perform};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// What the terminal engine asks of its host: replies to queries go back to the program.
#[derive(Clone)]
pub struct Proxy {
    events: mpsc::Sender<Event>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

/// Throughput of the output stream, for the proof of concept's measurements.
#[derive(Default)]
pub struct Stats {
    pub bytes: AtomicU64,
    pub first: Mutex<Option<Instant>>,
    pub last: Mutex<Option<Instant>>,
    /// Time spent parsing, in microseconds.
    pub parse_us: AtomicU64,
}

pub struct Shared {
    pub term: Mutex<Term<Proxy>>,
    /// Set by the reader when the screen changed; the UI clears it when it redraws.
    pub dirty: AtomicBool,
    pub exited: AtomicBool,
    /// Claude Code's latest status report (OSC 7501's payload), if any.
    pub status: Mutex<Option<String>>,
    pub stats: Stats,
}

impl Shared {
    pub fn term(&self) -> MutexGuard<'_, Term<Proxy>> {
        self.term.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Session {
    pub shared: Arc<Shared>,
    input: mpsc::Sender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    events: mpsc::Receiver<Event>,
    pub size: GridSize,
    pub title: Option<String>,
    pub bells: u32,
}

/// Picks Claude Code's status reports out of the stream. Claude Code asks `ESC ] 7501 ; ? BEL`
/// at startup and sends reports only to a terminal that answers the same; a report is
/// `ESC ] 7501 ; state=working:kind=…:title=<base64> BEL`, one per thread (`id=`, none for the
/// main one). The proof of concept keeps the main thread's state and kind.
struct StatusTap<'a> {
    shared: &'a Shared,
    reply: &'a mpsc::Sender<Vec<u8>>,
}

impl Perform for StatusTap<'_> {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.first() != Some(&&b"7501"[..]) {
            return;
        }
        let data: Vec<String> = params[1..].iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect();
        let data = data.join(";");
        if data.starts_with('?') {
            let _ = self.reply.send(b"\x1b]7501;?\x07".to_vec());
            return;
        }
        let field = |k: &str| data.split(':').find_map(|p| p.strip_prefix(k).and_then(|v| v.strip_prefix('=')).map(str::to_owned));
        if field("id").is_some() {
            return; // a subagent's report
        }
        let shown = match (field("state"), field("kind")) {
            (Some(state), _) if state == "clear" => None,
            (Some(state), Some(kind)) => Some(format!("{state} ({kind})")),
            (Some(state), None) => Some(state),
            (None, _) => return,
        };
        *self.shared.status.lock().unwrap_or_else(|e| e.into_inner()) = shown;
    }
    fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
}

/// `line` run through the shell (cmd.exe on Windows), as Switchyard starts `claude`.
fn command(line: &str) -> CommandBuilder {
    let mut cmd = if cfg!(windows) {
        let mut c = CommandBuilder::new("cmd.exe");
        c.args(["/D", "/C", line]);
        c
    } else {
        let mut c = CommandBuilder::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
        c.args(["-l", "-c", line]);
        c
    };
    if let Ok(dir) = std::env::current_dir() {
        cmd.cwd(dir);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd
}

impl Session {
    pub fn spawn(line: &str, size: GridSize, waker: Waker) -> std::io::Result<Session> {
        let err = |e: anyhow::Error| std::io::Error::other(e.to_string());
        let pty = native_pty_system()
            .openpty(PtySize { rows: size.rows as u16, cols: size.cols as u16, pixel_width: 0, pixel_height: 0 })
            .map_err(err)?;
        let mut child = pty.slave.spawn_command(command(line)).map_err(err)?;
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().map_err(err)?;
        let mut writer = pty.master.take_writer().map_err(err)?;

        let (events_tx, events) = mpsc::channel();
        let term = Term::new(Config::default(), &size, Proxy { events: events_tx });
        let shared = Arc::new(Shared {
            term: Mutex::new(term),
            dirty: AtomicBool::new(true),
            exited: AtomicBool::new(false),
            status: Mutex::new(None),
            stats: Stats::default(),
        });

        let (input, keys) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new().name("pty-writer".into()).spawn(move || {
            for bytes in keys {
                if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                    break;
                }
            }
        })?;

        // The program's exit is watched on its own: on Windows, ConPTY doesn't end the output
        // stream when the program exits, so a reader waiting for that end would wait forever.
        let s = shared.clone();
        let wake = waker.clone();
        std::thread::Builder::new().name("pty-child".into()).spawn(move || {
            let _ = child.wait();
            s.exited.store(true, Ordering::Release);
            s.dirty.store(true, Ordering::Release);
            wake();
        })?;

        let s = shared.clone();
        let reply = input.clone();
        std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            let mut tap = Parser::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let now = Instant::now();
                s.stats.first.lock().unwrap().get_or_insert(now);
                tap.advance(&mut StatusTap { shared: &s, reply: &reply }, &buf[..n]);
                // Small chunks under the lock, so a frame waiting to draw gets in between them.
                for chunk in buf[..n].chunks(8 * 1024) {
                    let mut term = s.term();
                    parser.advance(&mut *term, chunk);
                }
                s.stats.parse_us.fetch_add(now.elapsed().as_micros() as u64, Ordering::Relaxed);
                s.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
                *s.stats.last.lock().unwrap() = Some(Instant::now());
                s.dirty.store(true, Ordering::Release);
                waker();
            }
        })?;

        Ok(Session { shared, input, master: pty.master, events, size, title: None, bells: 0 })
    }

    /// Sends bytes to the program, as typed.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.input.send(bytes.into());
    }

    /// Pastes `text`, wrapped in bracketed-paste markers when the program asked for them
    /// (Claude Code does), so a multi-line paste stays one prompt.
    pub fn paste(&self, text: &str) {
        let bracketed = self.shared.term().mode().contains(TermMode::BRACKETED_PASTE);
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if bracketed {
            // A paste mustn't end the bracket early.
            let clean = text.replace("\x1b[201~", "");
            self.write(format!("\x1b[200~{clean}\x1b[201~"));
        } else {
            self.write(text);
        }
    }

    pub fn resize(&mut self, size: GridSize, cell: [f32; 2]) {
        if size == self.size || size.cols < 2 || size.rows < 1 {
            return;
        }
        self.size = size;
        self.shared.term().resize(size);
        let _ = self.master.resize(PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: (size.cols as f32 * cell[0]) as u16,
            pixel_height: (size.rows as f32 * cell[1]) as u16,
        });
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Handles what the terminal engine asked for since last time: replies to the program's
    /// queries (cursor position, colours, size), the title and the bell. True when something
    /// shown changed.
    pub fn pump_events(&mut self, cell: [f32; 2]) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::PtyWrite(text) => self.write(text),
                Event::Title(t) => {
                    self.title = Some(t);
                    changed = true;
                }
                Event::ResetTitle => {
                    self.title = None;
                    changed = true;
                }
                Event::Bell => {
                    self.bells += 1;
                    changed = true;
                }
                Event::ColorRequest(index, reply) => {
                    let rgb = self.shared.term().colors()[index].unwrap_or(alacritty_terminal::vte::ansi::Rgb { r: 0x16, g: 0x15, b: 0x13 });
                    self.write(reply(rgb));
                }
                Event::TextAreaSizeRequest(reply) => {
                    let size = WindowSize {
                        num_lines: self.size.rows as u16,
                        num_cols: self.size.cols as u16,
                        cell_width: cell[0] as u16,
                        cell_height: cell[1] as u16,
                    };
                    self.write(reply(size));
                }
                _ => {}
            }
        }
        changed
    }
}
