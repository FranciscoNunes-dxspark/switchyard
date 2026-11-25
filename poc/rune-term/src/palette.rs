//! Terminal colours: Switchyard's dark theme (src/themes.ts) as Rune colours, and how a cell's
//! colour (named, one of 256, or 24-bit) resolves to one.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor, Rgb};
use rune::Color;

/// The 16 ANSI colours, then foreground, background, cursor and selection (sRGB hex).
const DARK: [&str; 16] = [
    "#272522", "#e5736a", "#8fbf7f", "#e0b86b", "#7aa6da", "#c397d8", "#70c0b1", "#d8d6cc",
    "#726e67", "#f08a80", "#a6d394", "#f0cb82", "#94bbe8", "#d4ade6", "#8ad4c6", "#f7f4ec",
];
pub const BACKGROUND: &str = "#161513";
pub const FOREGROUND: &str = "#e8e4da";
pub const CURSOR: &str = "#f2b13c";
pub const SELECTION: &str = "#6fa8dc45";
/// The window around the terminal: Switchyard's surfaces and text.
pub const SURFACE: &str = "#1c1b19";
pub const HAIRLINE: &str = "#ffffff14";
pub const TEXT_2: &str = "#a9a79f";
pub const ACCENT: &str = "#f2b13c";

pub fn hex(s: &str) -> Color {
    rune::color::hex(s).unwrap_or(Color { r: 1.0, g: 0.0, b: 1.0, a: 1.0 })
}

fn from_rgb(c: Rgb) -> Color {
    let d = rune::color::srgb_decode;
    Color { r: d(c.r as f32 / 255.0), g: d(c.g as f32 / 255.0), b: d(c.b as f32 / 255.0), a: 1.0 }
}

/// The 256-colour table: the theme's 16, the 6×6×6 cube and the 24 greys, as xterm builds them.
pub struct Palette {
    table: [Color; 256],
    fg: Color,
    bg: Color,
}

impl Palette {
    pub fn dark() -> Palette {
        let mut table = [Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }; 256];
        for (i, h) in DARK.iter().enumerate() {
            table[i] = hex(h);
        }
        let level = |v: usize| if v == 0 { 0 } else { (v * 40 + 55) as u8 };
        for i in 0..216 {
            table[16 + i] = from_rgb(Rgb { r: level(i / 36), g: level((i / 6) % 6), b: level(i % 6) });
        }
        for i in 0..24 {
            let v = (8 + i * 10) as u8;
            table[232 + i] = from_rgb(Rgb { r: v, g: v, b: v });
        }
        Palette { table, fg: hex(FOREGROUND), bg: hex(BACKGROUND) }
    }

    pub fn background(&self) -> Color {
        self.bg
    }

    /// `c` as drawn, honouring colours the program changed with OSC 4/10/11 (`colors`).
    pub fn resolve(&self, c: TermColor, colors: &Colors) -> Color {
        match c {
            TermColor::Spec(rgb) => from_rgb(rgb),
            TermColor::Indexed(i) => colors[i as usize].map(from_rgb).unwrap_or(self.table[i as usize]),
            TermColor::Named(n) => {
                if let Some(rgb) = colors[n] {
                    return from_rgb(rgb);
                }
                match n {
                    NamedColor::Foreground | NamedColor::BrightForeground => self.fg,
                    NamedColor::DimForeground => self.fg.mix(self.bg, 0.33),
                    NamedColor::Background => self.bg,
                    NamedColor::Cursor => hex(CURSOR),
                    other => {
                        let i = other as usize;
                        if i < 16 {
                            self.table[i]
                        } else {
                            // Dim colours (DimBlack..DimWhite) follow the normal eight.
                            let base = i.saturating_sub(NamedColor::DimBlack as usize) % 8;
                            self.table[base].mix(self.bg, 0.33)
                        }
                    }
                }
            }
        }
    }
}
