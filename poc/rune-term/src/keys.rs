//! Keys as the bytes a terminal program expects (xterm's encoding).

use alacritty_terminal::term::TermMode;
use rune::{Key, KeyEvent};

/// What a key press means here: bytes for the program, or one of the window's own actions.
#[derive(Debug, PartialEq)]
pub enum Action {
    Send(Vec<u8>),
    Copy,
    /// Left to Rune, which reads the clipboard and calls the root `on_paste`.
    Paste,
    Ignore,
}

/// `cmd` here is the platform's shortcut modifier: ⌘ on macOS, Ctrl elsewhere.
pub fn action(e: &KeyEvent, mode: TermMode) -> Action {
    let m = &e.mods;
    let mac = cfg!(target_os = "macos");
    // Copy and paste: ⌘C / ⌘V on macOS, Ctrl+Shift+C / Ctrl+Shift+V elsewhere (plain Ctrl+C
    // and Ctrl+V belong to the program: interrupt, and Claude Code's image paste).
    let shortcut = if mac { m.cmd } else { m.ctrl && m.shift };
    if shortcut && e.is_command('c') {
        return Action::Copy;
    }
    if shortcut && e.is_command('v') {
        return Action::Paste;
    }
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let arrow = |c: char| {
        let modifier = xterm_modifier(m.shift, m.alt, m.ctrl);
        if modifier > 1 {
            format!("\x1b[1;{modifier}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let bytes: Vec<u8> = match &e.key {
        // Shift+Enter: a newline in Claude Code's prompt (Switchyard sends Meta+Enter too).
        Key::Enter if m.shift => b"\x1b\r".to_vec(),
        Key::Enter => b"\r".to_vec(),
        Key::Backspace if m.alt => b"\x1b\x7f".to_vec(),
        Key::Backspace if m.ctrl => b"\x08".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => arrow('A'),
        Key::Down => arrow('B'),
        Key::Right => arrow('C'),
        Key::Left => arrow('D'),
        Key::Home => arrow('H'),
        Key::End => arrow('F'),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Function(n) => function_key(*n),
        // Ctrl+letter (Rune reports it as Command(c) off macOS, with `ctrl` set) is a control
        // character; ⌘ chords on macOS are the app's, not the program's.
        Key::Command(c) if m.ctrl || !mac => match control_byte(*c) {
            Some(b) => alt_prefixed(m.alt, vec![b]),
            None => return Action::Ignore,
        },
        Key::Command(_) => return Action::Ignore,
        Key::Text(t) if m.ctrl && !m.alt && t.chars().count() == 1 => match control_byte(t.chars().next().unwrap()) {
            Some(b) => vec![b],
            None => t.as_bytes().to_vec(),
        },
        // Option on macOS types characters (é, ß); elsewhere Alt+key is ESC + key.
        Key::Text(t) => alt_prefixed(m.alt && !mac, t.as_bytes().to_vec()),
        _ => return Action::Ignore,
    };
    Action::Send(bytes)
}

fn alt_prefixed(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
fn xterm_modifier(shift: bool, alt: bool, ctrl: bool) -> u8 {
    1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8
}

fn control_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '8' | '?' => Some(0x7f),
        _ => None,
    }
}

fn function_key(n: u8) -> Vec<u8> {
    match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5..=12 => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize];
            format!("\x1b[{code}~").into_bytes()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Mods;

    fn key(key: Key, mods: Mods) -> KeyEvent {
        KeyEvent { key, mods }
    }
    fn send(a: Action) -> Vec<u8> {
        match a {
            Action::Send(b) => b,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn typing_and_editing_keys() {
        let none = Mods::default();
        assert_eq!(send(action(&key(Key::Text("é".into()), none), TermMode::empty())), "é".as_bytes());
        assert_eq!(send(action(&key(Key::Enter, none), TermMode::empty())), b"\r");
        assert_eq!(send(action(&key(Key::Enter, Mods { shift: true, ..none }), TermMode::empty())), b"\x1b\r");
        assert_eq!(send(action(&key(Key::Up, none), TermMode::empty())), b"\x1b[A");
        assert_eq!(send(action(&key(Key::Up, none), TermMode::APP_CURSOR)), b"\x1bOA");
        assert_eq!(send(action(&key(Key::Left, Mods { ctrl: true, cmd: !cfg!(target_os = "macos"), ..none }), TermMode::empty())), b"\x1b[1;5D");
    }

    #[test]
    fn ctrl_c_interrupts_and_ctrl_shift_c_copies() {
        let ctrl = Mods { ctrl: true, cmd: !cfg!(target_os = "macos"), ..Mods::default() };
        let c = if cfg!(target_os = "macos") { Key::Text("c".into()) } else { Key::Command('c') };
        assert_eq!(send(action(&key(c, ctrl), TermMode::empty())), [3]);
        if !cfg!(target_os = "macos") {
            let copy = Mods { shift: true, ..ctrl };
            assert_eq!(action(&key(Key::Command('c'), copy), TermMode::empty()), Action::Copy);
        }
    }
}
