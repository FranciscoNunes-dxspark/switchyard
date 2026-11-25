//! The terminal's screen as Rune grid rows. A row is rebuilt only when what it shows changed
//! (its cells, the selection over it, the cursor on it); otherwise the same `Rc<GridRow>` goes
//! back to Rune, which then keeps that row's shaped text and drawing.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color as TermColor, CursorShape};
use rune::{Color, GridRow, Span, TextStyle};

use crate::palette::{self, Palette};
use crate::session::Proxy;

pub const FONT_SIZE: f32 = 13.0;
pub const LINE_HEIGHT: f32 = 18.0;

pub fn style() -> TextStyle {
    TextStyle::new(FONT_SIZE, 400.0).family(rune::Family::SystemMono).lh(LINE_HEIGHT)
}

#[derive(Default)]
pub struct Screen {
    pub rows: Vec<Rc<GridRow>>,
    keys: Vec<u64>,
    /// The cursor's cell on screen, when it's shown.
    pub cursor: Option<(u16, u16)>,
    pub display_offset: usize,
    pub history: usize,
    /// Rows rebuilt by the last update, for the measurements.
    pub rebuilt: usize,
}

struct Style {
    fg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl Screen {
    pub fn update(&mut self, term: &alacritty_terminal::Term<Proxy>, pal: &Palette, focused: bool) {
        let content = term.renderable_content();
        let rows = term.screen_lines();
        let cols = term.columns();
        let offset = content.display_offset as i32;
        let colors = content.colors;
        let show_cursor = content.mode.contains(TermMode::SHOW_CURSOR) && content.display_offset == 0;
        let cursor = content.cursor.point;
        let cursor_shape = content.cursor.shape;
        let selection = content.selection;
        self.cursor = None;
        self.display_offset = content.display_offset;
        self.history = term.history_size();

        // Group the visible cells by screen row.
        let mut lines: Vec<Vec<&Cell>> = vec![Vec::with_capacity(cols); rows];
        for cell in content.display_iter {
            let row = (cell.point.line.0 + offset) as usize;
            if row < rows {
                lines[row].push(cell.cell);
            }
        }

        self.rows.resize_with(rows, || Rc::new(GridRow::new("", Vec::new(), style())));
        self.keys.resize(rows, u64::MAX);
        self.rebuilt = 0;
        let sel_color = palette::hex(palette::SELECTION);
        for (row, cells) in lines.iter().enumerate() {
            let grid_line = Line(row as i32 - offset);
            let selected: Option<(usize, usize)> = selection.and_then(|s| {
                let cols_in = (0..cols).filter(|&c| s.contains(Point::new(grid_line, Column(c))));
                let (mut lo, mut hi) = (usize::MAX, 0);
                for c in cols_in {
                    lo = lo.min(c);
                    hi = hi.max(c + 1);
                }
                (lo < hi).then_some((lo, hi))
            });
            let cursor_here = (show_cursor && cursor.line == grid_line).then_some(cursor.column.0);
            if let Some(c) = cursor_here {
                self.cursor = Some((c as u16, row as u16));
            }

            let mut h = DefaultHasher::new();
            for c in cells {
                (c.c, c.flags.bits()).hash(&mut h);
                color_key(c.fg).hash(&mut h);
                color_key(c.bg).hash(&mut h);
                if let Some(z) = c.zerowidth() {
                    z.hash(&mut h);
                }
            }
            (selected, cursor_here, focused, cursor_shape as u8).hash(&mut h);
            let key = h.finish();
            if self.keys[row] == key {
                continue;
            }
            self.keys[row] = key;
            self.rebuilt += 1;
            let cursor_cell = cursor_here.filter(|_| cursor_shape != CursorShape::Hidden);
            self.rows[row] = Rc::new(build_row(cells, pal, colors, selected.map(|r| (r, sel_color)), cursor_cell, focused));
        }
    }
}

fn color_key(c: TermColor) -> (u8, u32) {
    match c {
        TermColor::Named(n) => (0, n as u32),
        TermColor::Indexed(i) => (1, i as u32),
        TermColor::Spec(rgb) => (2, (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32),
    }
}

fn build_row(
    cells: &[&Cell],
    pal: &Palette,
    colors: &alacritty_terminal::term::color::Colors,
    selected: Option<((usize, usize), Color)>,
    cursor: Option<usize>,
    focused: bool,
) -> GridRow {
    let base = style();
    let mut text = String::with_capacity(cells.len());
    let mut spans: Vec<Span> = Vec::new();
    let mut pinned: Vec<(u16, u16, String, Color, bool)> = Vec::new();
    let mut fills: Vec<(u16, u16, Color)> = Vec::new();
    let default_bg = pal.background();
    let cursor_color = palette::hex(palette::CURSOR);

    let mut run: Option<(usize, Style)> = None;
    let mut bg_run: Option<(u16, Color)> = None;
    let flush = |spans: &mut Vec<Span>, start: usize, end: usize, s: &Style| {
        if end > start {
            let mut span = Span::new(start, end).color(s.fg);
            if s.bold {
                span = span.bold();
            }
            if s.italic {
                span = span.italic();
            }
            if s.underline {
                span.underline = Some(s.fg);
            }
            spans.push(span);
        }
    };

    for (col, cell) in cells.iter().enumerate() {
        let flags = cell.flags;
        let mut fg = pal.resolve(cell.fg, colors);
        let mut bg = pal.resolve(cell.bg, colors);
        if flags.contains(Flags::BOLD) && matches!(cell.fg, TermColor::Named(n) if (n as usize) < 8) {
            // Bold uses the bright colour, as xterm does by default.
            if let TermColor::Named(n) = cell.fg {
                fg = pal.resolve(TermColor::Indexed(n as u8 + 8), colors);
            }
        }
        if flags.contains(Flags::DIM) {
            fg = fg.mix(default_bg, 0.4);
        }
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        // The cursor: a block in the cursor colour, its character in the background colour.
        if cursor == Some(col) {
            if focused {
                bg = cursor_color;
                fg = default_bg;
            } else {
                // Unfocused: an outline would need a shape; a dimmer block says the same.
                bg = cursor_color.alpha(0.35);
            }
        }
        let col16 = col as u16;

        // Backgrounds as runs of columns.
        let want = (bg != default_bg).then_some(bg);
        if bg_run.map(|(_, c)| c) != want {
            if let Some((start, c)) = bg_run.take() {
                fills.push((start, col16, c));
            }
            bg_run = want.map(|c| (col16, c));
        }

        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            text.push(' ');
            continue;
        }
        let mut glyph = String::new();
        glyph.push(cell.c);
        if let Some(z) = cell.zerowidth() {
            glyph.extend(z.iter());
        }
        let wide = flags.contains(Flags::WIDE_CHAR);
        // ASCII sits on the shaped line; anything else is pinned to its column, so a glyph
        // from a fallback font of another width can't push the columns after it.
        if !wide && glyph.len() == 1 {
            let style = Style { fg, bold: flags.contains(Flags::BOLD), italic: flags.contains(Flags::ITALIC), underline: flags.intersects(Flags::ALL_UNDERLINES) };
            let start = text.len();
            match &mut run {
                Some((_, s)) if s.fg == style.fg && s.bold == style.bold && s.italic == style.italic && s.underline == style.underline => {}
                _ => {
                    if let Some((st, s)) = run.take() {
                        flush(&mut spans, st, start, &s);
                    }
                    run = Some((start, style));
                }
            }
            text.push(cell.c);
        } else {
            if let Some((st, s)) = run.take() {
                flush(&mut spans, st, text.len(), &s);
            }
            text.push(' ');
            pinned.push((col16, if wide { 2 } else { 1 }, glyph, fg, flags.contains(Flags::BOLD)));
        }
    }
    if let Some((st, s)) = run.take() {
        flush(&mut spans, st, text.len(), &s);
    }
    if let Some((start, c)) = bg_run {
        fills.push((start, cells.len() as u16, c));
    }

    let wraps = cells.last().is_some_and(|c| c.flags.contains(Flags::WRAPLINE));
    let mut row = GridRow::new(text, spans, base).wraps(wraps);
    for (start, end, c) in fills {
        row = row.fill(start, end, c);
    }
    if let Some(((start, end), c)) = selected {
        row = row.fill(start as u16, end as u16, c);
    }
    for (col, width, glyph, fg, bold) in pinned {
        let st = if bold { TextStyle { weight: 600.0, ..base } } else { base };
        row = row.cell(col, width, glyph, st, fg);
    }
    row
}
