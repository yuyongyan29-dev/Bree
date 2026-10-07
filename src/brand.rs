//! Static, typed rendering of a compact icon from the approved 32 × 32 pixel grid.
//! The source ANSI is decoded offline; no escape sequences reach the terminal.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
};

pub(crate) const WIDTH: u16 = 12;
pub(crate) const HEIGHT: u16 = 6;
const SOURCE_WIDTH: usize = 32;
const TRANSPARENT: u8 = 6;
const RGB: [Color; 6] = [
    Color::Rgb(40, 40, 40),
    Color::Rgb(77, 77, 77),
    Color::Rgb(134, 134, 132),
    Color::Rgb(185, 183, 176),
    Color::Rgb(222, 220, 213),
    Color::Rgb(249, 246, 239),
];
const INDEXED: [Color; 6] = [
    Color::Indexed(236),
    Color::Indexed(239),
    Color::Indexed(244),
    Color::Indexed(250),
    Color::Indexed(253),
    Color::Indexed(231),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorDepth {
    None,
    Rgb,
    Indexed256,
}

impl ColorDepth {
    pub(crate) fn detect(term: Option<&str>, colorterm: Option<&str>, no_color: bool) -> Self {
        let term = term.unwrap_or("").trim().to_ascii_lowercase();
        if no_color || term.is_empty() || term == "dumb" {
            return Self::None;
        }
        let declared_rgb = [
            "direct",
            "truecolor",
            "24bit",
            "ghostty",
            "kitty",
            "wezterm",
        ]
        .iter()
        .any(|value| term.contains(value));
        let declared_256 = term.contains("256color");
        if (term == "ansi" || term.starts_with("vt") || term.starts_with("linux"))
            && !declared_rgb
            && !declared_256
        {
            return Self::None;
        }
        let colorterm = colorterm.unwrap_or("").trim().to_ascii_lowercase();
        if declared_rgb || matches!(colorterm.as_str(), "truecolor" | "24bit") {
            Self::Rgb
        } else if declared_256 {
            Self::Indexed256
        } else {
            Self::None
        }
    }

    pub(crate) fn from_environment() -> Self {
        Self::detect(
            std::env::var("TERM").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var_os("NO_COLOR").is_some(),
        )
    }

    fn palette(self) -> [Color; 6] {
        match self {
            Self::Rgb => RGB,
            Self::Indexed256 => INDEXED,
            Self::None => [Color::Reset; 6],
        }
    }
}

const fn decode_grid(source: &[u8]) -> [u8; 1024] {
    assert!(
        source.len() == 32 * 33,
        "mascot grid must be 32 rows of 32 pixels"
    );
    let mut pixels = [TRANSPARENT; 1024];
    let mut row = 0;
    while row < 32 {
        let mut column = 0;
        while column < 32 {
            pixels[row * 32 + column] = match source[row * 33 + column] {
                b'.' => TRANSPARENT,
                value @ b'0'..=b'5' => value - b'0',
                _ => panic!("invalid mascot palette index"),
            };
            column += 1;
        }
        assert!(
            source[row * 33 + 32] == b'\n',
            "mascot rows must end with a newline"
        );
        row += 1;
    }
    pixels
}

const SOURCE_PIXELS: [u8; 1024] = decode_grid(include_bytes!("../assets/bree-mascot.txt"));

const fn compact_grid(source: &[u8; 1024]) -> [u8; WIDTH as usize * HEIGHT as usize * 2] {
    let mut pixels = [TRANSPARENT; WIDTH as usize * HEIGHT as usize * 2];
    let mut y = 0;
    while y < HEIGHT as usize * 2 {
        let source_y = (2 * y + 1) * SOURCE_WIDTH / (HEIGHT as usize * 4);
        let mut x = 0;
        while x < WIDTH as usize {
            let source_x = (2 * x + 1) * SOURCE_WIDTH / (WIDTH as usize * 2);
            pixels[y * WIDTH as usize + x] = source[source_y * SOURCE_WIDTH + source_x];
            x += 1;
        }
        y += 1;
    }
    pixels
}

// Pixel-center sampling retains the approved palette and transparency without
// interpolation, image dependencies, or terminal escape sequences at runtime.
const PIXELS: [u8; WIDTH as usize * HEIGHT as usize * 2] = compact_grid(&SOURCE_PIXELS);

pub(crate) fn render(frame: &mut Frame<'_>, area: Rect, depth: ColorDepth) {
    if depth == ColorDepth::None || area.width < WIDTH || area.height < HEIGHT {
        return;
    }
    let palette = depth.palette();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let top = PIXELS[(y * 2 * WIDTH + x) as usize];
            let bottom = PIXELS[((y * 2 + 1) * WIDTH + x) as usize];
            let (glyph, foreground, background) = match (top, bottom) {
                (TRANSPARENT, TRANSPARENT) => (" ", Color::Reset, Color::Reset),
                (TRANSPARENT, bottom) => ("▄", palette[bottom as usize], Color::Reset),
                (top, TRANSPARENT) => ("▀", palette[top as usize], Color::Reset),
                (top, bottom) => ("▀", palette[top as usize], palette[bottom as usize]),
            };
            frame.buffer_mut()[(area.x + x, area.y + y)]
                .set_symbol(glyph)
                .set_style(Style::default().fg(foreground).bg(background));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn capability_detection_uses_only_declared_colors_and_respects_disabled_terminals() {
        for term in [
            None,
            Some(""),
            Some("dumb"),
            Some("ansi"),
            Some("vt100"),
            Some("linux"),
        ] {
            assert_eq!(
                ColorDepth::detect(term, Some("truecolor"), false),
                ColorDepth::None
            );
        }
        for term in [
            "xterm-256color",
            "screen-256color",
            "xterm-ghostty",
            "xterm-direct",
        ] {
            assert_eq!(
                ColorDepth::detect(Some(term), Some("truecolor"), true),
                ColorDepth::None
            );
        }
        assert_eq!(
            ColorDepth::detect(Some("xterm-256color"), None, false),
            ColorDepth::Indexed256
        );
        assert_eq!(
            ColorDepth::detect(Some("xterm-256color"), Some("truecolor"), false),
            ColorDepth::Rgb
        );
        assert_eq!(
            ColorDepth::detect(Some("xterm"), Some("24bit"), false),
            ColorDepth::Rgb
        );
        assert_eq!(
            ColorDepth::detect(Some("xterm-ghostty"), None, false),
            ColorDepth::Rgb
        );
        assert_eq!(
            ColorDepth::detect(Some("xterm-direct"), None, false),
            ColorDepth::Rgb
        );
        assert_eq!(
            ColorDepth::detect(Some("xterm"), None, false),
            ColorDepth::None
        );
    }

    #[test]
    fn compact_icon_preserves_source_tones_and_transparency_as_typed_cells() {
        assert_eq!(
            SOURCE_PIXELS
                .iter()
                .filter(|pixel| **pixel != TRANSPARENT)
                .count(),
            555
        );
        for (index, count) in [64, 69, 7, 44, 87, 284].into_iter().enumerate() {
            assert_eq!(
                SOURCE_PIXELS
                    .iter()
                    .filter(|pixel| **pixel == index as u8)
                    .count(),
                count
            );
        }
        assert_eq!(PIXELS.len(), 12 * 12);
        assert!(PIXELS.iter().all(|pixel| *pixel <= TRANSPARENT));
        assert!(
            PIXELS[..WIDTH as usize]
                .iter()
                .all(|pixel| *pixel == TRANSPARENT)
        );
        assert!(PIXELS.contains(&0));
        assert!(PIXELS.contains(&1));
        assert!(PIXELS.contains(&5));
        for depth in [ColorDepth::Rgb, ColorDepth::Indexed256] {
            let mut terminal = Terminal::new(TestBackend::new(36, 18)).unwrap();
            terminal
                .draw(|frame| render(frame, Rect::new(2, 1, WIDTH, HEIGHT), depth))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let palette = depth.palette();
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let top = PIXELS[(y * 2 * WIDTH + x) as usize];
                    let bottom = PIXELS[((y * 2 + 1) * WIDTH + x) as usize];
                    let cell = &buffer[(2 + x, 1 + y)];
                    if top == TRANSPARENT || bottom == TRANSPARENT {
                        assert_eq!(cell.bg, Color::Reset);
                    } else {
                        assert_eq!(cell.symbol(), "▀");
                        assert_eq!(cell.fg, palette[top as usize]);
                        assert_eq!(cell.bg, palette[bottom as usize]);
                    }
                    if top == TRANSPARENT && bottom == TRANSPARENT {
                        assert_eq!(cell.symbol(), " ");
                        assert_eq!(cell.fg, Color::Reset);
                    }
                }
            }
            for x in 0..36 {
                assert_eq!(buffer[(x, 0)].bg, Color::Reset);
                assert_eq!(buffer[(x, 17)].bg, Color::Reset);
            }
        }
    }

    #[test]
    fn compact_icon_does_not_paint_disabled_or_insufficient_areas() {
        for (depth, area) in [
            (ColorDepth::None, Rect::new(2, 1, WIDTH, HEIGHT)),
            (ColorDepth::Rgb, Rect::new(2, 1, WIDTH - 1, HEIGHT)),
            (ColorDepth::Rgb, Rect::new(2, 1, WIDTH, HEIGHT - 1)),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
            terminal.draw(|frame| render(frame, area, depth)).unwrap();
            assert!(terminal.backend().buffer().content.iter().all(|cell| {
                cell.symbol() == " " && cell.fg == Color::Reset && cell.bg == Color::Reset
            }));
        }
    }
}
