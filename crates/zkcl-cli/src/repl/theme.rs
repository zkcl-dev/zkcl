use std::env;
use std::fmt::Write as _;
use std::time::Duration;

use nu_ansi_term::{Color, Style};

#[derive(Clone, Copy)]
pub(super) struct ReplTheme {
    pub color: bool,
    pub truecolor: bool,
}

impl ReplTheme {
    pub(super) fn new(color: bool) -> Self {
        let truecolor = color
            && env::var("COLORTERM")
                .map(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"))
                .unwrap_or(false);
        Self { color, truecolor }
    }

    pub(super) fn paint(&self, text: &str, style: Style) -> String {
        if self.color && style != Style::new() {
            style.paint(text).to_string()
        } else {
            text.to_owned()
        }
    }

    fn rgb_or(&self, r: u8, g: u8, b: u8, fixed: u8) -> Color {
        if self.truecolor {
            Color::Rgb(r, g, b)
        } else {
            Color::Fixed(fixed)
        }
    }

    // --- Prompt & Chrome ---

    pub(super) fn brand(&self) -> Style {
        Style::new().fg(self.rgb_or(120, 128, 148, 244))
    }

    pub(super) fn package(&self) -> Style {
        Style::new().fg(self.rgb_or(122, 162, 247, 75)).bold()
    }

    pub(super) fn prompt_arrow(&self) -> Style {
        Style::new().fg(self.rgb_or(120, 128, 148, 244))
    }

    pub(super) fn multiline_bar(&self) -> Style {
        Style::new().fg(self.rgb_or(92, 99, 112, 242))
    }

    pub(super) fn badge(&self) -> Style {
        Style::new().fg(self.rgb_or(224, 175, 104, 179))
    }

    pub(super) fn muted(&self) -> Style {
        Style::new().fg(Color::DarkGray)
    }

    pub(super) fn subtle(&self) -> Style {
        Style::new().fg(Color::DarkGray)
    }

    pub(super) fn heading(&self) -> Style {
        Style::new().fg(self.rgb_or(125, 207, 255, 81)).bold()
    }

    // --- Semantic & Syntax Highlighting ---

    pub(super) fn paren(&self) -> Style {
        Style::new().fg(self.rgb_or(136, 144, 166, 245))
    }

    pub(super) fn keyword(&self) -> Style {
        Style::new().fg(self.rgb_or(187, 154, 247, 141))
    }

    pub(super) fn operator(&self) -> Style {
        Style::new().fg(self.rgb_or(187, 154, 247, 141)).bold()
    }

    pub(super) fn macro_style(&self) -> Style {
        Style::new().fg(self.rgb_or(125, 207, 255, 81)).bold()
    }

    pub(super) fn function(&self) -> Style {
        Style::new().fg(self.rgb_or(122, 162, 247, 75))
    }

    pub(super) fn parameter(&self) -> Style {
        Style::new().fg(self.rgb_or(137, 221, 255, 117))
    }

    pub(super) fn variable(&self) -> Style {
        Style::new().fg(self.rgb_or(224, 175, 104, 179))
    }

    pub(super) fn constant(&self) -> Style {
        Style::new().fg(self.rgb_or(224, 175, 104, 179))
    }

    pub(super) fn type_style(&self) -> Style {
        Style::new().fg(self.rgb_or(115, 218, 202, 79))
    }

    pub(super) fn string(&self) -> Style {
        Style::new().fg(self.rgb_or(158, 206, 106, 114))
    }

    pub(super) fn number(&self) -> Style {
        Style::new().fg(self.rgb_or(255, 158, 100, 209))
    }

    pub(super) fn comment(&self) -> Style {
        Style::new().fg(Color::DarkGray).italic()
    }

    pub(super) fn reader_prefix(&self) -> Style {
        Style::new().fg(self.rgb_or(187, 154, 247, 141))
    }

    pub(super) fn opaque(&self) -> Style {
        Style::new().fg(Color::DarkGray).italic()
    }

    pub(super) fn command(&self) -> Style {
        Style::new().fg(self.rgb_or(125, 207, 255, 81)).bold()
    }

    pub(super) fn command_arg(&self) -> Style {
        Style::new().fg(self.rgb_or(192, 202, 245, 189))
    }

    pub(super) fn rainbow_paren(&self, depth: usize) -> Style {
        let color = match depth % 4 {
            0 => self.rgb_or(136, 144, 166, 245),
            1 => self.rgb_or(115, 218, 202, 79),
            2 => self.rgb_or(224, 175, 104, 179),
            _ => self.rgb_or(187, 154, 247, 141),
        };
        Style::new().fg(color)
    }

    pub(super) fn matched_paren(&self) -> Style {
        Style::new().fg(Color::LightCyan).bold()
    }

    pub(super) fn error(&self) -> Style {
        Style::new().fg(Color::Red).bold()
    }

    pub(super) fn error_underline(&self) -> Style {
        Style::new().fg(Color::Red).bold().underline()
    }

    pub(super) fn incomplete(&self) -> Style {
        Style::new().fg(Color::Yellow)
    }

    pub(super) fn active_param(&self) -> Style {
        Style::new().fg(Color::Cyan).bold().underline()
    }

    pub(super) fn result_prefix(&self) -> Style {
        Style::new().fg(Color::Green).bold()
    }

    pub(super) fn menu_selected_bg(&self) -> Color {
        self.rgb_or(42, 56, 78, 237)
    }

    pub(super) fn menu_text(&self) -> Style {
        Style::new().fg(Color::LightGray)
    }

    pub(super) fn menu_selected_text(&self) -> Style {
        Style::new()
            .fg(Color::White)
            .on(self.menu_selected_bg())
            .bold()
    }

    pub(super) fn menu_match(&self) -> Style {
        Style::new().fg(Color::Cyan).bold().underline()
    }

    pub(super) fn menu_selected_match(&self) -> Style {
        Style::new()
            .fg(Color::LightCyan)
            .on(self.menu_selected_bg())
            .bold()
            .underline()
    }

    pub(super) fn menu_description(&self) -> Style {
        Style::new().fg(Color::DarkGray).italic()
    }
}

pub(super) fn format_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        format!("{:.1} µs", duration.as_secs_f64() * 1_000_000.0)
    } else if nanos < 1_000_000_000 {
        format!("{:.2} ms", duration.as_secs_f64() * 1_000.0)
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

pub(super) fn format_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.1} KiB", b / KIB)
    } else if b < GIB {
        format!("{:.2} MiB", b / MIB)
    } else {
        format!("{:.2} GiB", b / GIB)
    }
}

/// Lightweight Lisp code highlighter for macroexpansions, formatted code, and documentation snippets.
pub(super) fn highlight_lisp_output(text: &str, theme: &ReplTheme) -> String {
    if !theme.color || text.is_empty() {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len() + 32);
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push_str(&theme.paint(&text[start..i], theme.string()));
            }
            b';' => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                out.push_str(&theme.paint(&text[start..i], theme.comment()));
            }
            b'(' | b')' => {
                out.push_str(&theme.paint(&text[i..i + 1], theme.paren()));
                i += 1;
            }
            b'#' if i + 1 < bytes.len() => match bytes[i + 1] {
                b'<' => {
                    let start = i;
                    i += 2;
                    let mut nested = 1usize;
                    while i < bytes.len() && nested > 0 {
                        match bytes[i] {
                            b'<' if bytes.get(i.saturating_sub(1)) == Some(&b'#') => nested += 1,
                            b'>' => nested -= 1,
                            b'\n' => break,
                            _ => {}
                        }
                        i += 1;
                    }
                    out.push_str(&theme.paint(&text[start..i], theme.opaque()));
                }
                b'\\' => {
                    let start = i;
                    i += 2;
                    if i < bytes.len() {
                        let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
                        i += ch_len;
                        while i < bytes.len() && is_atom_char(bytes[i]) {
                            i += 1;
                        }
                    }
                    out.push_str(&theme.paint(&text[start..i], theme.string()));
                }
                b'(' => {
                    out.push_str(&theme.paint("#", theme.reader_prefix()));
                    out.push_str(&theme.paint("(", theme.paren()));
                    i += 2;
                }
                b'\'' | b':' => {
                    out.push_str(&theme.paint(&text[i..i + 2], theme.reader_prefix()));
                    i += 2;
                }
                _ => {
                    out.push_str(&theme.paint("#", theme.reader_prefix()));
                    i += 1;
                }
            },
            b'\'' | b'`' | b',' => {
                let len = if bytes[i] == b',' && bytes.get(i + 1) == Some(&b'@') {
                    2
                } else {
                    1
                };
                out.push_str(&theme.paint(&text[i..i + len], theme.reader_prefix()));
                i += len;
            }
            b if is_atom_char(b) => {
                let start = i;
                while i < bytes.len() && is_atom_char(bytes[i]) {
                    let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
                    i += ch_len;
                }
                let atom = &text[start..i];
                if let Some(style) = classify_output_atom(atom, theme) {
                    out.push_str(&theme.paint(atom, style));
                } else {
                    out.push_str(atom);
                }
            }
            _ => {
                let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
                let _ = write!(out, "{}", &text[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    out
}

fn is_atom_char(byte: u8) -> bool {
    !matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | b'(' | b')' | b'"' | b'\'' | b'`' | b',' | b';'
    )
}

fn classify_output_atom(atom: &str, theme: &ReplTheme) -> Option<Style> {
    if atom.starts_with(':') {
        return Some(theme.keyword());
    }
    if atom.eq_ignore_ascii_case("NIL") || atom.eq_ignore_ascii_case("T") {
        return Some(theme.constant());
    }
    if looks_like_lisp_number(atom) {
        return Some(theme.number());
    }
    None
}

fn looks_like_lisp_number(atom: &str) -> bool {
    if atom.parse::<i64>().is_ok() {
        return true;
    }
    if atom.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) && atom.parse::<f64>().is_ok() {
        return true;
    }
    if let Some((num, den)) = atom.split_once('/') {
        return !num.is_empty()
            && !den.is_empty()
            && num.parse::<i64>().is_ok()
            && den.parse::<i64>().is_ok();
    }
    false
}
