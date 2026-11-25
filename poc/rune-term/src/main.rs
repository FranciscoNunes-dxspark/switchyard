//! Proof of concept: one Claude Code session in a Rune window.
//!
//! What it checks, before any decision to move Switchyard's interface off the web view:
//! typing while Claude is busy, heavy output, selection and copy, scrollback, resizing,
//! composed input (accents, CJK), and Claude Code's own status reports (OSC 7501).
//!
//! `cargo run --release` runs `claude` in the current folder; `SWITCHYARD_POC_CMD="…"` runs
//! another command line instead. Rune's headless flags work too (`--snap out.png`); with them,
//! `SWITCHYARD_POC_WAIT_MS` waits that long for output before the first frame.

mod keys;
mod palette;
mod screen;
mod session;

use std::sync::atomic::Ordering;
use std::time::Duration;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use rune::{App, AppSpec, Cx, Drag, El, Ime, KeyEvent, Phase, TextStyle, Tx, WheelEvent, col, el, grid, row, text};

use crate::palette::{Palette, hex};
use crate::screen::Screen;
use crate::session::{GridSize, Session};

const PAD: f32 = 10.0;

struct State {
    session: Option<Session>,
    error: Option<String>,
    pal: Palette,
    screen: Screen,
    /// One cell's width and height in points, measured from the font.
    cell: [f32; 2],
    focused: bool,
    /// What an input method is composing, drawn at the cursor until it commits.
    preedit: Option<String>,
    /// Wheel travel not yet a whole line.
    wheel: f32,
    status: Option<String>,
    /// Measurements shown in the header.
    stats: String,
}

fn init() -> State {
    State {
        session: None,
        error: None,
        pal: Palette::dark(),
        screen: Screen::default(),
        cell: [0.0, screen::LINE_HEIGHT],
        focused: true,
        preedit: None,
        wheel: 0.0,
        status: None,
        stats: String::new(),
    }
}

fn setup(s: &mut State, waker: rune::Waker) {
    s.cell = measure_cell();
    let line = std::env::var("SWITCHYARD_POC_CMD").unwrap_or_else(|_| "claude".into());
    match Session::spawn(&line, GridSize { cols: 100, rows: 30 }, waker) {
        Ok(session) => s.session = Some(session),
        Err(e) => s.error = Some(format!("Couldn't start the program: {e}")),
    }
    // Headless captures: let the program write before the first frame.
    if let Some(ms) = std::env::var("SWITCHYARD_POC_WAIT_MS").ok().and_then(|v| v.parse().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// A cell is as wide as the font's monospaced advance.
fn measure_cell() -> [f32; 2] {
    let sample = "M".repeat(200);
    let (w, _) = rune::text::measure_inline(&sample, &[], &[], screen::style(), None);
    [(w / 200.0).max(1.0), screen::LINE_HEIGHT]
}

fn poll(s: &mut State, _t: f64, _tx: &mut Tx) -> bool {
    let Some(session) = s.session.as_mut() else { return false };
    let mut changed = session.pump_events(s.cell);
    if session.shared.dirty.swap(false, Ordering::AcqRel) {
        let term = session.shared.term();
        s.screen.update(&term, &s.pal, s.focused);
        drop(term);
        s.status = session.shared.status.lock().unwrap().clone();
        s.stats = stats(session, &s.screen);
        changed = true;
    }
    changed
}

fn stats(session: &Session, screen: &Screen) -> String {
    let st = &session.shared.stats;
    let bytes = st.bytes.load(Ordering::Relaxed);
    let span = match (*st.first.lock().unwrap(), *st.last.lock().unwrap()) {
        (Some(a), Some(b)) => (b - a).as_secs_f64(),
        _ => 0.0,
    };
    let parse_ms = st.parse_us.load(Ordering::Relaxed) as f64 / 1000.0;
    let mb = bytes as f64 / 1_048_576.0;
    format!(
        "{mb:.1} MB in {span:.2} s · parsed in {parse_ms:.0} ms · {} rows redrawn · {} lines of history{}",
        screen.rebuilt,
        screen.history,
        if screen.display_offset > 0 { format!(" · scrolled back {}", screen.display_offset) } else { String::new() }
    )
}

/// The cell under a point in the terminal box, and which half of it.
fn cell_at(s: &State, x: f32, y: f32) -> Option<(Point, Side)> {
    let session = s.session.as_ref()?;
    let [cw, ch] = s.cell;
    let fx = ((x - PAD) / cw).max(0.0);
    let col = (fx as usize).min(session.size.cols - 1);
    let row = (((y - PAD) / ch).max(0.0) as usize).min(session.size.rows - 1);
    let side = if fx.fract() > 0.5 { Side::Right } else { Side::Left };
    Some((Point::new(Line(row as i32 - s.screen.display_offset as i32), Column(col)), side))
}

fn on_drag(s: &mut State, d: &mut Drag, _tx: &mut Tx) {
    let Some((point, side)) = cell_at(s, d.x, d.y) else { return };
    let Some(session) = s.session.as_ref() else { return };
    let mut term = session.shared.term();
    match d.phase {
        Phase::Down => {
            let ty = match d.clicks {
                2 => SelectionType::Semantic,
                3.. => SelectionType::Lines,
                _ => SelectionType::Simple,
            };
            term.selection = Some(Selection::new(ty, point, side));
        }
        Phase::Move | Phase::Up => {
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
        }
    }
    drop(term);
    session.shared.dirty.store(true, Ordering::Release);
}

fn on_wheel(s: &mut State, e: WheelEvent, _tx: &mut Tx) -> bool {
    let Some(session) = s.session.as_ref() else { return false };
    s.wheel += e.dy;
    let lines = (s.wheel / s.cell[1]).trunc();
    if lines != 0.0 {
        s.wheel -= lines * s.cell[1];
        session.shared.term().scroll_display(Scroll::Delta(lines as i32));
        session.shared.dirty.store(true, Ordering::Release);
    }
    true
}

fn on_key(s: &mut State, e: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(session) = s.session.as_ref() else { return false };
    let mode = *session.shared.term().mode();
    match keys::action(e, mode) {
        keys::Action::Send(bytes) => {
            let mut term = session.shared.term();
            if term.grid().display_offset() > 0 {
                term.scroll_display(Scroll::Bottom);
                session.shared.dirty.store(true, Ordering::Release);
            }
            drop(term);
            session.write(bytes);
            true
        }
        keys::Action::Copy => {
            if let Some(text) = session.shared.term().selection_to_string() {
                tx.copy(text);
            }
            true
        }
        keys::Action::Paste | keys::Action::Ignore => false,
    }
}

fn on_ime(s: &mut State, ime: &Ime, _tx: &mut Tx) -> bool {
    apply_ime(s, ime);
    true
}

/// An input method's composition: shown at the cursor while it's open, typed when it commits.
fn apply_ime(s: &mut State, ime: &Ime) {
    match ime {
        Ime::Preedit(text, ..) => s.preedit = (!text.is_empty()).then(|| text.clone()),
        Ime::Commit(text) => {
            s.preedit = None;
            if let Some(session) = s.session.as_ref() {
                session.write(text.as_bytes().to_vec());
            }
        }
        Ime::Enabled | Ime::Disabled => s.preedit = None,
        _ => {}
    }
}

fn on_layout(s: &mut State, rect: [f32; 4]) {
    let [cw, ch] = s.cell;
    if cw <= 0.0 {
        return;
    }
    let cols = ((rect[2] - 2.0 * PAD) / cw).floor().max(2.0) as usize;
    let rows = ((rect[3] - 2.0 * PAD) / ch).floor().max(1.0) as usize;
    if let Some(session) = s.session.as_mut() {
        session.resize(GridSize { cols, rows }, s.cell);
    }
}

fn label(t: &str, color: &str) -> El {
    text(t).font(TextStyle::new(12.0, 500.0)).color(hex(color)).nowrap()
}

fn view(s: &State, _cx: &mut Cx<State>) -> El {
    let bg = hex(palette::BACKGROUND);
    let program = s.session.as_ref().and_then(|x| x.title.clone()).unwrap_or_else(|| "Claude Code".into());
    let status = match (&s.error, s.session.as_ref().map(|x| x.shared.exited.load(Ordering::Acquire))) {
        (Some(e), _) => e.clone(),
        (None, Some(true)) => "Ended".into(),
        _ => s.status.clone().unwrap_or_else(|| "no status yet".into()),
    };
    let header = row()
        .w_full()
        .h(36.0)
        .padding(10.0)
        .gap(14.0)
        .fill(hex(palette::SURFACE))
        .child(label("Switchyard · Rune", palette::ACCENT))
        .child(label(&program, palette::FOREGROUND))
        .child(label(&status, palette::TEXT_2))
        .child(el().grow(1.0))
        .child(label(&s.stats, palette::TEXT_2));
    let line = el().w_full().h(1.0).fill(hex(palette::HAIRLINE));

    let mut term = el()
        .key("term")
        .w_full()
        .grow(1.0)
        .fill(bg)
        .clip_soft(0.0)
        .cursor(rune::Cursor::Text)
        .on_layout(on_layout)
        .on_wheel(on_wheel)
        .on_drag(on_drag);
    if let Some(session) = s.session.as_ref() {
        let [cw, ch] = s.cell;
        let size = session.size;
        let mut g = grid(size.cols as u16, cw, ch, s.screen.rows.clone())
            .w(size.cols as f32 * cw)
            .h(size.rows as f32 * ch)
            .absolute(PAD, PAD);
        if let Some((c, r)) = s.screen.cursor {
            g = g.grid_caret(c, r);
        }
        term = term.child(g);
        // Where an input method composes: its candidate window opens here, and what it has
        // so far shows over the cursor until it commits.
        if let Some((c, r)) = s.screen.cursor {
            let mut caret = el().absolute(PAD + c as f32 * cw, PAD + r as f32 * ch).h(ch).ime_cursor();
            caret = match &s.preedit {
                Some(p) => caret
                    .fill(hex(palette::SURFACE))
                    .child(text(p).font(screen::style()).color(hex(palette::FOREGROUND)).nowrap()),
                None => caret.w(cw),
            };
            term = term.child(caret);
        }
    }

    col()
        .w_full()
        .h_full()
        .fill(bg)
        .on_paste(|s: &mut State, text: &str, _tx: &mut Tx| {
            if let Some(session) = s.session.as_ref() {
                session.paste(text);
            }
        })
        .on_window_focus(|s: &mut State, focused: bool, _tx: &mut Tx| {
            s.focused = focused;
            if let Some(session) = s.session.as_ref() {
                session.shared.dirty.store(true, Ordering::Release);
            }
        })
        .child(header)
        .child(line)
        .child(term)
}

fn main() {
    let spec = AppSpec::<State> {
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        min_size: Some((420.0, 240.0)),
        ..AppSpec::new("Switchyard (Rune proof of concept)", (980.0, 640.0), init, view)
    };
    App::new(spec).on_ime(on_ime).run();
}

#[cfg(test)]
mod tests;
