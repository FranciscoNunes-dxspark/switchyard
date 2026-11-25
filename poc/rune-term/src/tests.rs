//! The proof of concept's checklist, each against a real program in a real pseudo-terminal,
//! driven through Rune's headless runtime (the same layout, input routing and renderer
//! bookkeeping as a window, without one). Times are wall-clock on this machine.
//!
//! `cargo test --release -- --nocapture --test-threads=1` prints the measurements.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use rune::{Env, Ime, Key, KeyEvent, Mods, Runtime};

use super::*;

const SIZE: (f32, f32) = (900.0, 560.0);

fn start(line: &str) -> Runtime<State> {
    let mut s = init();
    s.cell = measure_cell();
    s.session = Some(Session::spawn(line, GridSize { cols: 100, rows: 30 }, Arc::new(|| {})).unwrap());
    let mut rt = Runtime::new(s, view, SIZE, Env::new(true, 2.0));
    rt.poll = Some(poll);
    rt.on_key = Some(on_key);
    rt.frame(0.0);
    rt
}

/// Draws frames until `done` holds (or `limit` passes), returning how long each frame took.
fn frames_until(rt: &mut Runtime<State>, limit: Duration, done: impl Fn(&State) -> bool) -> Vec<f64> {
    let start = Instant::now();
    let mut times = Vec::new();
    while start.elapsed() < limit {
        let t = Instant::now();
        rt.frame(1.0 / 60.0);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        if done(&rt.state) {
            return times;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    panic!("timed out after {limit:?}; screen:\n{}", screen_text(&rt.state));
}

fn screen_text(s: &State) -> String {
    let session = s.session.as_ref().unwrap();
    let term = session.shared.term();
    let mut out = String::new();
    for row in 0..term.screen_lines() {
        let line = Line(row as i32 - term.grid().display_offset() as i32);
        let text: String = (0..term.columns())
            .map(|c| &term.grid()[Point::new(line, Column(c))])
            .filter(|cell| !cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER))
            .map(|cell| cell.c)
            .collect();
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

fn shows(s: &State, needle: &str) -> bool {
    screen_text(s).contains(needle)
}

fn exited(s: &State) -> bool {
    s.session.as_ref().unwrap().shared.exited.load(Ordering::Acquire)
}

fn key(k: Key) -> KeyEvent {
    KeyEvent { key: k, mods: Mods::default() }
}

fn ctrl_shift(c: char) -> KeyEvent {
    let mods = Mods { ctrl: true, shift: true, cmd: !cfg!(target_os = "macos"), ..Mods::default() };
    KeyEvent { key: Key::Command(c), mods }
}

fn percentile(v: &[f64], p: f64) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn term_origin(rt: &Runtime<State>) -> (f32, f32) {
    let r = rt.presented("term").expect("the terminal box").0;
    (r[0] + PAD, r[1] + PAD)
}

/// The window keeps drawing and keys still reach the program while it floods the screen:
/// typed text waits in the terminal until the program reads it, and nothing is lost.
#[test]
#[cfg(unix)]
fn typing_during_a_flood_of_output() {
    let mut rt = start("seq 1 3000000; read line; echo \"got: $line\"; sleep 20");
    // Let the flood start, then type while it runs.
    let mut times = frames_until(&mut rt, Duration::from_secs(10), |s| shows(s, "000"));
    for c in "hello".chars() {
        rt.key(key(Key::Text(c.to_string())));
        times.extend(frames_until(&mut rt, Duration::from_secs(1), |_| true));
    }
    rt.key(key(Key::Enter));
    times.extend(frames_until(&mut rt, Duration::from_secs(60), |s| shows(s, "got: hello")));
    let st = &rt.state.session.as_ref().unwrap().shared.stats;
    let mb = st.bytes.load(Ordering::Relaxed) as f64 / 1_048_576.0;
    println!(
        "flood: {mb:.1} MB, {} frames, frame time p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
        times.len(),
        percentile(&times, 0.5),
        percentile(&times, 0.95),
        percentile(&times, 1.0)
    );
}

/// 400,000 lines arrive and are all parsed: the last one is on screen, history is kept up to
/// its limit, and parsing keeps up.
#[test]
#[cfg(unix)]
fn four_hundred_thousand_lines() {
    let t = Instant::now();
    let mut rt = start("seq 1 400000");
    let times = frames_until(&mut rt, Duration::from_secs(60), |s| exited(s) && shows(s, "400000"));
    let total = t.elapsed().as_secs_f64();
    let s = &rt.state;
    let st = &s.session.as_ref().unwrap().shared.stats;
    println!(
        "400k lines: {:.1} MB, all on screen after {total:.2} s, parsing took {:.0} ms in all, {} frames (p95 {:.1} ms), history {} lines",
        st.bytes.load(Ordering::Relaxed) as f64 / 1_048_576.0,
        st.parse_us.load(Ordering::Relaxed) as f64 / 1000.0,
        times.len(),
        percentile(&times, 0.95),
        s.screen.history
    );
    assert!(shows(s, "400000"));
    assert_eq!(s.screen.history, 10_000, "scrollback keeps its limit");
}

/// Dragging across a word selects it; Ctrl+Shift+C (⌘C on macOS) copies exactly that.
#[test]
#[cfg(unix)]
fn select_and_copy() {
    let mut rt = start("printf 'alpha beta gamma\\n'; sleep 20");
    frames_until(&mut rt, Duration::from_secs(10), |s| shows(s, "gamma"));
    let (x0, y0) = term_origin(&rt);
    let [cw, ch] = rt.state.cell;
    // "beta" is columns 6..10 of row 0.
    let y = y0 + ch * 0.5;
    // A frame between pointer events, as a window draws between them.
    rt.pointer_down(x0 + cw * 6.1, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_move(x0 + cw * 8.0, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_move(x0 + cw * 9.9, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_up(x0 + cw * 9.9, y);
    rt.frame(1.0 / 60.0);
    let copy = if cfg!(target_os = "macos") { KeyEvent { key: Key::Command('c'), mods: Mods { cmd: true, ..Mods::default() } } } else { ctrl_shift('c') };
    rt.key(copy);
    // The clipboard write goes out with the next frame.
    rt.frame(1.0 / 60.0);
    let copied = rt.copy_request.take().map(|w| w.content().text().to_string());
    println!("copied {copied:?}");
    assert_eq!(copied.as_deref(), Some("beta"));
}

/// The wheel scrolls back through history; typing returns to the bottom.
#[test]
#[cfg(unix)]
fn scrollback_and_back() {
    let mut rt = start("seq 1 300; cat");
    frames_until(&mut rt, Duration::from_secs(10), |s| shows(s, "\n300\n"));
    let (x0, y0) = term_origin(&rt);
    let ch = rt.state.cell[1];
    // Content follows the fingers: a positive dy moves it down, showing older lines. (Native
    // windows deliver wheels as `wheel_event`, which reaches `on_wheel`.)
    rt.wheel_event(rune::WheelEvent::new(0.0, ch * 100.0).at(x0 + 50.0, y0 + 50.0));
    frames_until(&mut rt, Duration::from_secs(2), |s| s.screen.display_offset > 0);
    let back = rt.state.screen.display_offset;
    let text = screen_text(&rt.state);
    println!("scrolled back {back} lines; top row now {:?}", text.lines().next());
    assert!(back >= 90, "scrolled {back}");
    rt.key(key(Key::Text("x".into())));
    frames_until(&mut rt, Duration::from_secs(2), |s| s.screen.display_offset == 0);
}

/// A smaller window gives the program a smaller terminal, and it sees the new size.
#[test]
#[cfg(unix)]
fn resize_reaches_the_program() {
    let mut rt = start("sleep 1.5; stty size; sleep 20");
    rt.frame(1.0 / 60.0);
    let before = rt.state.session.as_ref().unwrap().size;
    rt.viewport = (600.0, 400.0);
    frames_until(&mut rt, Duration::from_secs(1), |s| s.session.as_ref().unwrap().size != before);
    let after = rt.state.session.as_ref().unwrap().size;
    let expect = format!("{} {}", after.rows, after.cols);
    frames_until(&mut rt, Duration::from_secs(10), |s| shows(s, &expect));
    println!("resized {}x{} -> {}x{}; the program saw \"{expect}\"", before.cols, before.rows, after.cols, after.rows);
}

/// A paste is bracketed when the program asked for it, so a multi-line paste stays one prompt.
#[test]
#[cfg(unix)]
fn bracketed_paste() {
    let mut rt = start("printf '\\033[?2004h'; stty -echo; cat -v");
    frames_until(&mut rt, Duration::from_secs(5), |s| {
        s.session.as_ref().unwrap().shared.term().mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
    });
    rt.paste("one\ntwo");
    rt.key(key(Key::Enter));
    frames_until(&mut rt, Duration::from_secs(5), |s| shows(s, "^[[201~"));
    println!("{}", screen_text(&rt.state).trim());
    assert!(shows(&rt.state, "^[[200~one"));
}

/// Accented characters typed directly, and text an input method composes (dead keys, CJK).
#[test]
#[cfg(unix)]
fn accents_and_composed_input() {
    let mut rt = start("cat");
    rt.key(key(Key::Text("é".into())));
    rt.frame(1.0 / 60.0);
    rt.key(key(Key::Text("ñ".into())));
    rt.frame(1.0 / 60.0);
    // What App::on_ime hears from a composing input method (Rune's own tests cover routing).
    let state = &mut rt.state;
    apply_ime(state, &Ime::Preedit("にほ".into(), None, Vec::new()));
    assert_eq!(state.preedit.as_deref(), Some("にほ"));
    apply_ime(state, &Ime::Commit("日本".into()));
    assert_eq!(state.preedit, None);
    rt.key(key(Key::Enter));
    frames_until(&mut rt, Duration::from_secs(5), |s| shows(s, "éñ日本"));
}

/// Claude Code's status protocol: the terminal answers its query, and reports are read.
#[test]
#[cfg(unix)]
fn claude_status_reports() {
    let mut rt = start(
        "stty -icanon -echo; printf '\\033]7501;?\\007'; head -c 9 | od -An -c; \
         printf '\\033]7501;state=blocked:kind=permission\\007'; sleep 20",
    );
    frames_until(&mut rt, Duration::from_secs(5), |s| s.status.as_deref() == Some("blocked (permission)"));
    let text = screen_text(&rt.state);
    println!("reply as the program read it: {:?}", text.lines().next());
    assert!(text.contains("7   5   0   1   ;   ?"), "{text}");
}


// Windows: the same pane over ConPTY, with commands cmd.exe and PowerShell have.

/// ConPTY starts a program and its output reaches the screen.
#[test]
#[cfg(windows)]
fn windows_output_reaches_the_screen() {
    let mut rt = start("echo hello from conpty");
    frames_until(&mut rt, Duration::from_secs(20), |s| shows(s, "hello from conpty"));
}

/// Typed keys reach a program reading a line, accents included.
#[test]
#[cfg(windows)]
fn windows_typing() {
    let mut rt = start("powershell -NoProfile -Command \"$l = Read-Host 'name'; Write-Output ('got: ' + $l); Start-Sleep 20\"");
    frames_until(&mut rt, Duration::from_secs(30), |s| shows(s, "name"));
    for c in ["c", "a", "f", "é"] {
        rt.key(key(Key::Text(c.into())));
        rt.frame(1.0 / 60.0);
    }
    rt.key(key(Key::Enter));
    frames_until(&mut rt, Duration::from_secs(10), |s| shows(s, "got: café"));
}

/// A flood of output over ConPTY: all of it arrives and frames stay short.
#[test]
#[cfg(windows)]
fn windows_flood() {
    let t = Instant::now();
    let mut rt = start("powershell -NoProfile -Command \"1..200000\"");
    let times = frames_until(&mut rt, Duration::from_secs(120), |s| exited(s) && shows(s, "200000"));
    let st = &rt.state.session.as_ref().unwrap().shared.stats;
    println!(
        "windows flood: {:.1} MB, done after {:.2} s, parsing {:.0} ms, {} frames (p95 {:.1} ms, max {:.1} ms)",
        st.bytes.load(Ordering::Relaxed) as f64 / 1_048_576.0,
        t.elapsed().as_secs_f64(),
        st.parse_us.load(Ordering::Relaxed) as f64 / 1000.0,
        times.len(),
        percentile(&times, 0.95),
        percentile(&times, 1.0)
    );
}
