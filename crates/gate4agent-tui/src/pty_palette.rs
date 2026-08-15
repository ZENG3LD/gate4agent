use uzor_tui::{Color, Modifier, TerminalBuffer};

use crate::app::PtyColorMode;

pub const GATE_FG: Color = Color::Rgb(205, 214, 244);
pub const TERM_BG: Color = Color::Rgb(17, 17, 27);

const ANSI: [Color; 16] = [
    Color::Rgb(69, 71, 90),
    Color::Rgb(243, 139, 168),
    Color::Rgb(166, 227, 161),
    Color::Rgb(249, 226, 175),
    Color::Rgb(137, 180, 250),
    Color::Rgb(245, 194, 231),
    Color::Rgb(148, 226, 213),
    Color::Rgb(186, 194, 222),
    Color::Rgb(88, 91, 112),
    Color::Rgb(243, 139, 168),
    Color::Rgb(166, 227, 161),
    Color::Rgb(249, 226, 175),
    Color::Rgb(137, 180, 250),
    Color::Rgb(203, 166, 247),
    Color::Rgb(148, 226, 213),
    Color::Rgb(166, 173, 200),
];

pub fn apply_pty_palette(buf: &mut TerminalBuffer, mode: PtyColorMode) {
    if mode == PtyColorMode::Inherited {
        return;
    }
    for row in 0..buf.height() {
        for column in 0..buf.width() {
            let cell = buf.get_mut(column, row);
            let foreground = gate_foreground(cell.style.fg);
            if cell.style.modifiers.contains(Modifier::REVERSE) {
                cell.style.fg = TERM_BG;
                cell.style.bg = foreground;
            } else {
                cell.style.fg = foreground;
                cell.style.bg = TERM_BG;
            }
        }
    }
}

fn gate_foreground(color: Color) -> Color {
    match color {
        Color::Reset => GATE_FG,
        Color::Black => ANSI[0],
        Color::Red => ANSI[1],
        Color::Green => ANSI[2],
        Color::Yellow => ANSI[3],
        Color::Blue => ANSI[4],
        Color::Magenta => ANSI[5],
        Color::Cyan => ANSI[6],
        Color::White => ANSI[7],
        Color::Gray => ANSI[7],
        Color::DarkGray => ANSI[8],
        Color::LightRed => ANSI[9],
        Color::LightGreen => ANSI[10],
        Color::LightYellow => ANSI[11],
        Color::LightBlue => ANSI[12],
        Color::LightMagenta => ANSI[13],
        Color::LightCyan => ANSI[14],
        Color::Indexed(index @ 0..=15) => ANSI[index as usize],
        Color::Indexed(index) => nearest_swatch(indexed_rgb(index)),
        Color::Rgb(red, green, blue) => nearest_swatch((red, green, blue)),
    }
}

fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    if index < 16 {
        return rgb(ANSI[index as usize]);
    }
    if index <= 231 {
        let cube = index - 16;
        let component = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
        return (
            component(cube / 36),
            component((cube % 36) / 6),
            component(cube % 6),
        );
    }
    let grey = 8 + (index - 232) * 10;
    (grey, grey, grey)
}

fn nearest_swatch(target: (u8, u8, u8)) -> Color {
    ANSI
        .iter()
        .copied()
        .min_by_key(|color| distance_squared(target, rgb(*color)))
        .unwrap_or(GATE_FG)
}

fn distance_squared(left: (u8, u8, u8), right: (u8, u8, u8)) -> u32 {
    let red = left.0 as i32 - right.0 as i32;
    let green = left.1 as i32 - right.1 as i32;
    let blue = left.2 as i32 - right.2 as i32;
    (red * red + green * green + blue * blue) as u32
}

fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(red, green, blue) => (red, green, blue),
        _ => unreachable!("gate swatches are RGB"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uzor_tui::{Modifier, Style};

    #[test]
    fn inherited_preserves_native_pty_cells_exactly() {
        let mut buf = TerminalBuffer::new(1, 1);
        let cell = buf.get_mut(0, 0);
        cell.symbol = "K".into();
        cell.style = Style {
            fg: Color::Rgb(1, 2, 3),
            bg: Color::Indexed(21),
            modifiers: Modifier::BOLD | Modifier::UNDERLINE,
        };
        let expected = cell.clone();
        apply_pty_palette(&mut buf, PtyColorMode::Inherited);
        assert_eq!(buf.get(0, 0), &expected);
    }

    #[test]
    fn override_maps_reset_and_all_backgrounds_to_gate_surface() {
        let mut buf = TerminalBuffer::new(2, 1);
        buf.get_mut(0, 0).style = Style::default();
        buf.get_mut(1, 0).style = Style {
            fg: Color::White,
            bg: Color::Rgb(0, 51, 102),
            modifiers: Modifier::DIM,
        };
        apply_pty_palette(&mut buf, PtyColorMode::GateOverride);
        assert_eq!(buf.get(0, 0).style.fg, GATE_FG);
        assert_eq!(buf.get(0, 0).style.bg, TERM_BG);
        assert_eq!(buf.get(1, 0).style.fg, ANSI[7]);
        assert_eq!(buf.get(1, 0).style.bg, TERM_BG);
        assert_eq!(buf.get(1, 0).style.modifiers, Modifier::DIM);
    }

    #[test]
    fn override_maps_ansi_indexed_and_rgb_to_fixed_swatches() {
        assert_eq!(gate_foreground(Color::Red), ANSI[1]);
        assert_eq!(gate_foreground(Color::Gray), ANSI[7]);
        assert_eq!(gate_foreground(Color::DarkGray), ANSI[8]);
        assert_eq!(gate_foreground(Color::LightRed), ANSI[9]);
        assert_eq!(gate_foreground(Color::LightGreen), ANSI[10]);
        assert_eq!(gate_foreground(Color::LightYellow), ANSI[11]);
        assert_eq!(gate_foreground(Color::LightBlue), ANSI[12]);
        assert_eq!(gate_foreground(Color::LightMagenta), ANSI[13]);
        assert_eq!(gate_foreground(Color::LightCyan), ANSI[14]);
        assert_eq!(gate_foreground(Color::Indexed(12)), ANSI[12]);
        assert_eq!(gate_foreground(Color::Rgb(137, 180, 250)), ANSI[4]);
        assert_eq!(gate_foreground(Color::Indexed(196)), ANSI[1]);
    }

    #[test]
    fn reverse_is_pre_swapped_so_effective_background_stays_gate_surface() {
        let mut buf = TerminalBuffer::new(1, 1);
        buf.get_mut(0, 0).style = Style {
            fg: Color::Red,
            bg: Color::Blue,
            modifiers: Modifier::REVERSE
                | Modifier::BOLD
                | Modifier::ITALIC
                | Modifier::UNDERLINE,
        };
        apply_pty_palette(&mut buf, PtyColorMode::GateOverride);
        let style = buf.get(0, 0).style;
        assert_eq!(style.fg, TERM_BG);
        assert_eq!(style.bg, ANSI[1]);
        assert_eq!(
            style.modifiers,
            Modifier::REVERSE | Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINE
        );
        let effective_background = style.fg;
        assert_eq!(effective_background, TERM_BG);
    }
}
