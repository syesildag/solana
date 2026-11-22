//! Console rendering for the portfolio watcher's per-tick panels (Portfolio / Watchlist /
//! Positions).
//!
//! Why the panels bypass `tracing`: tracing-subscriber ≥ 0.3.20 escapes every ESC byte in
//! a message (a terminal-injection guard), so colour codes passed through `info!` print as
//! a literal `\x1b[32m`. Turning that guard off globally would be wrong here — token
//! symbols come from on-chain metadata, and with adopt-all-tokens an airdropped spam token
//! can put control bytes in a logged symbol. So the panels are written with `print!` (one
//! call per panel ⇒ one stdout-lock hold ⇒ no interleaving with tracing lines), every
//! untrusted string goes through [`clean`], and tracing keeps its sanitizer on.
//!
//! Colour is decided once per process ([`color_enabled`]): `LOG_COLOR=always|never|auto`
//! (default auto), auto honours `NO_COLOR` and is on only when stdout is a terminal — so a
//! redirect to `~/Library/Logs/portfolio-watcher.log` stays plain text. Nothing parses
//! these panels; the JSONL audit (`momentum_actions.jsonl`) is the machine record.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Whether panels (and the tracing formatter) emit ANSI colour. Decided once.
pub fn color_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        decide_color(
            std::env::var("LOG_COLOR").ok().as_deref(),
            std::env::var_os("NO_COLOR").is_some(),
            std::io::stdout().is_terminal(),
        )
    })
}

fn decide_color(mode: Option<&str>, no_color: bool, tty: bool) -> bool {
    match mode.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => !no_color && tty,
    }
}

/// An SGR parameter string (`"1;32"` = bold green). Empty = no styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style(&'static str);

impl Style {
    pub const PLAIN: Style = Style("");
    pub const BOLD: Style = Style("1");
    pub const DIM: Style = Style("2");
    pub const RED: Style = Style("31");
    pub const GREEN: Style = Style("32");
    pub const YELLOW: Style = Style("33");
    pub const CYAN: Style = Style("36");
    pub const BOLD_RED: Style = Style("1;31");
    pub const BOLD_GREEN: Style = Style("1;32");
    pub const BOLD_CYAN: Style = Style("1;36");
}

/// Wrap `text` in `style` when colour is on.
pub fn paint(text: &str, style: Style) -> String {
    paint_with(text, style, color_enabled())
}

fn paint_with(text: &str, style: Style, on: bool) -> String {
    if !on || style.0.is_empty() || text.is_empty() {
        text.to_string()
    } else {
        format!("\x1b[{}m{}\x1b[0m", style.0, text)
    }
}

/// Green for gains, red for losses, plain for flat.
pub fn sign_style(v: f64) -> Style {
    if v > 0.0 {
        Style::GREEN
    } else if v < 0.0 {
        Style::RED
    } else {
        Style::PLAIN
    }
}

/// Make an untrusted string (token symbol from on-chain metadata / an API) safe to print:
/// drop every control character (C0, DEL and C1 — `char::is_control`) and cap the length
/// so one absurd symbol cannot blow a table's column out.
pub fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(12).collect()
}

/// Display width. Every glyph these panels use (`€ σ Σ × — ● ▰ ▱ │ ─`) is one column wide,
/// so a char count is exact; [`clean`] strips the zero-width control bytes beforehand.
fn width(s: &str) -> usize {
    s.chars().count()
}

/// `▰▰▰▱▱▱` — `frac` of `cells` filled, clamped to [0, 1]; NaN renders empty.
pub fn bar(frac: f64, cells: usize) -> String {
    let f = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
    let n = ((f * cells as f64).round() as usize).min(cells);
    format!("{}{}", "▰".repeat(n), "▱".repeat(cells - n))
}

/// `1105.61` → `1,105.61` (`dp` decimals, comma thousands, sign kept).
pub fn thousands(v: f64, dp: usize) -> String {
    let s = format!("{:.*}", dp, v.abs());
    let (int, frac) = s.split_once('.').map_or((s.as_str(), None), |(i, f)| (i, Some(f)));
    let mut out = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    if v < 0.0 && out.chars().any(|c| c.is_ascii_digit() && c != '0') {
        out.insert(0, '-');
    }
    out
}

/// `€1,105.61` / `-€217.05`.
pub fn eur(v: f64) -> String {
    money('€', v)
}

/// `$1,105.61` / `-$217.05`.
pub fn usd(v: f64) -> String {
    money('$', v)
}

/// `+$12.30` / `-$4.10` / `$0.00` — a P&L amount, sign always shown.
pub fn signed_usd(v: f64) -> String {
    let s = usd(v);
    if v > 0.0 && s != "$0.00" { format!("+{s}") } else { s }
}

fn money(sym: char, v: f64) -> String {
    let body = thousands(v.abs(), 2);
    if v < 0.0 && body != "0.00" { format!("-{sym}{body}") } else { format!("{sym}{body}") }
}

/// A token price with precision that follows its magnitude: `$89.9887`, `$2.9969`,
/// `$0.004210`, `$1,234.56`.
pub fn price(v: f64) -> String {
    unit_price('$', v)
}

/// [`price`] in euros: `€81.8825`.
pub fn eur_price(v: f64) -> String {
    unit_price('€', v)
}

fn unit_price(sym: char, v: f64) -> String {
    let a = v.abs();
    let dp = if a >= 1000.0 {
        2
    } else if a >= 0.1 {
        4
    } else if a >= 0.001 {
        6
    } else {
        8
    };
    format!("{sym}{}", thousands(v, dp))
}

/// `$546` / `$52k` / `$1.3M` — order-of-magnitude volume.
pub fn compact_usd(v: f64) -> String {
    if v >= 1e6 {
        format!("${:.1}M", v / 1e6)
    } else if v >= 1e3 {
        format!("${:.0}k", v / 1e3)
    } else {
        format!("${v:.0}")
    }
}

/// `+1.42%` / `-0.20%`.
pub fn pct(v: f64, dp: usize) -> String {
    format!("{:+.*}%", dp, v)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// One table cell: plain text plus the style applied AFTER padding, so column widths are
/// computed on what the eye sees, never on escape bytes.
#[derive(Debug, Clone)]
pub struct Cell {
    text: String,
    style: Style,
}

impl Cell {
    pub fn new(text: impl Into<String>) -> Self {
        Cell { text: text.into(), style: Style::PLAIN }
    }
    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Cell { text: text.into(), style }
    }
    /// An empty cell.
    pub fn blank() -> Self {
        Cell::new("")
    }
}

impl From<String> for Cell {
    fn from(s: String) -> Self {
        Cell::new(s)
    }
}

impl From<&str> for Cell {
    fn from(s: &str) -> Self {
        Cell::new(s)
    }
}

/// A column-aligned table with a dim header row. Rows shorter than the header are padded
/// with blanks; the last column is never right-padded (no trailing whitespace).
pub struct Table {
    cols: Vec<(String, Align)>,
    rows: Vec<Vec<Cell>>,
}

const GAP: &str = "  ";

impl Table {
    pub fn new(cols: &[(&str, Align)]) -> Self {
        Table { cols: cols.iter().map(|(h, a)| (h.to_string(), *a)).collect(), rows: Vec::new() }
    }

    pub fn row(&mut self, cells: Vec<Cell>) {
        self.rows.push(cells);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Header + rows, one string per line.
    pub fn render(&self, on: bool) -> Vec<String> {
        let n = self.cols.len();
        let mut w: Vec<usize> = self.cols.iter().map(|(h, _)| width(h)).collect();
        for r in &self.rows {
            for (i, c) in r.iter().take(n).enumerate() {
                w[i] = w[i].max(width(&c.text));
            }
        }
        let line = |cells: Vec<(&str, Style)>| -> String {
            let mut out = String::new();
            for (i, (text, style)) in cells.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(GAP);
                }
                let pad = w[i].saturating_sub(width(text));
                let last = i + 1 == n;
                match self.cols[i].1 {
                    Align::Right => {
                        out.push_str(&" ".repeat(pad));
                        out.push_str(&paint_with(text, style, on));
                    }
                    Align::Left => {
                        out.push_str(&paint_with(text, style, on));
                        if !last {
                            out.push_str(&" ".repeat(pad));
                        }
                    }
                }
            }
            out.trim_end().to_string()
        };
        let mut out = Vec::with_capacity(self.rows.len() + 1);
        out.push(line(self.cols.iter().map(|(h, _)| (h.as_str(), Style::DIM)).collect()));
        for r in &self.rows {
            let cells = (0..n)
                .map(|i| r.get(i).map_or(("", Style::PLAIN), |c| (c.text.as_str(), c.style)))
                .collect();
            out.push(line(cells));
        }
        out
    }
}

/// Strip SGR sequences — for measuring a line that is already painted.
fn visible_width(s: &str) -> usize {
    let mut n = 0;
    let mut in_esc = false;
    for ch in s.chars() {
        match (in_esc, ch) {
            (false, '\x1b') => in_esc = true,
            (true, 'm') => in_esc = false,
            (true, _) => {}
            (false, _) => n += 1,
        }
    }
    n
}

/// Frame a panel:
///
/// ```text
/// ╭─ PORTFOLIO · 17:36:44 ───────────────────────────
/// │ asset  amount …
/// │ SOL    0.2120 …
/// ╰─ total €1,105.61 · from peak -€217.05 (16.4%)
/// ```
///
/// `title` and `footer` may already carry colour (painted by the caller with [`paint`]).
pub fn render_panel(title: &str, clock: &str, lines: &[String], footer: Option<&str>, on: bool) -> String {
    let body_w = lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
    let head = format!("╭─ {} · {} ", paint_with(title, Style::BOLD_CYAN, on), paint_with(clock, Style::DIM, on));
    let head_w = visible_width(&head);
    let rule_w = (body_w + 2).max(head_w + 3).min(160);
    let rule = "─".repeat(rule_w.saturating_sub(head_w));
    let mut out = format!("{head}{}\n", paint_with(&rule, Style::DIM, on));
    let gutter = paint_with("│", Style::DIM, on);
    for l in lines {
        out.push_str(&gutter);
        out.push(' ');
        out.push_str(l);
        out.push('\n');
    }
    match footer {
        Some(f) => out.push_str(&format!("{} {f}\n", paint_with("╰─", Style::DIM, on))),
        None => out.push_str(&format!("{}\n", paint_with("╰─", Style::DIM, on))),
    }
    out
}

/// Render and print a panel in ONE `print!` (atomic w.r.t. other stdout writers).
pub fn emit_panel(title: &str, table: &Table, footer: Option<&str>) {
    let on = color_enabled();
    let clock = chrono::Local::now().format("%H:%M:%S").to_string();
    print!("{}", render_panel(title, &clock, &table.render(on), footer, on));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_decision_honours_mode_then_no_color_then_tty() {
        assert!(decide_color(Some("always"), true, false), "always wins over NO_COLOR and pipes");
        assert!(!decide_color(Some("never"), false, true), "never wins over a TTY");
        assert!(decide_color(None, false, true), "auto: TTY ⇒ colour");
        assert!(!decide_color(None, false, false), "auto: file/pipe ⇒ plain");
        assert!(!decide_color(Some("auto"), true, true), "auto: NO_COLOR ⇒ plain");
        assert!(decide_color(Some(" ALWAYS "), false, false), "case/space-insensitive");
    }

    #[test]
    fn paint_is_identity_when_off_or_plain() {
        assert_eq!(paint_with("x", Style::GREEN, false), "x");
        assert_eq!(paint_with("x", Style::PLAIN, true), "x");
        assert_eq!(paint_with("x", Style::GREEN, true), "\x1b[32mx\x1b[0m");
    }

    #[test]
    fn clean_strips_c0_c1_and_del_and_caps_length() {
        assert_eq!(clean("HY\x1b[31mPE"), "HY[31mPE");
        assert_eq!(clean("a\u{9b}b\u{7f}c\u{7}"), "abc");
        assert_eq!(clean("ABCDEFGHIJKLMNOP"), "ABCDEFGHIJKL");
        assert_eq!(clean("wNEAR"), "wNEAR");
    }

    #[test]
    fn bar_clamps_and_rounds() {
        assert_eq!(bar(0.0, 4), "▱▱▱▱");
        assert_eq!(bar(0.5, 4), "▰▰▱▱");
        assert_eq!(bar(2.0, 4), "▰▰▰▰");
        assert_eq!(bar(-1.0, 4), "▱▱▱▱");
        assert_eq!(bar(f64::NAN, 4), "▱▱▱▱");
    }

    #[test]
    fn number_formats() {
        assert_eq!(thousands(1105.614, 2), "1,105.61");
        assert_eq!(thousands(-1234567.0, 0), "-1,234,567");
        assert_eq!(thousands(999.0, 0), "999");
        assert_eq!(thousands(-0.001, 2), "0.00", "no negative zero");
        assert_eq!(eur(1105.61), "€1,105.61");
        assert_eq!(eur(-217.05), "-€217.05");
        assert_eq!(usd(-0.001), "$0.00");
        assert_eq!(price(89.988657), "$89.9887");
        assert_eq!(eur_price(0.9092), "€0.9092");
        assert_eq!(price(0.0042101), "$0.004210");
        assert_eq!(price(0.000012346), "$0.00001235");
        assert_eq!(price(1234.5), "$1,234.50");
        assert_eq!(compact_usd(546.0), "$546");
        assert_eq!(compact_usd(52_400.0), "$52k");
        assert_eq!(compact_usd(1_340_000.0), "$1.3M");
        assert_eq!(pct(1.4236, 2), "+1.42%");
        assert_eq!(pct(-0.2, 2), "-0.20%");
    }

    #[test]
    fn table_aligns_on_visible_width_even_when_coloured() {
        let mut t = Table::new(&[("token", Align::Left), ("value", Align::Right), ("note", Align::Left)]);
        t.row(vec![Cell::new("SOL"), Cell::styled("13.60", Style::GREEN), Cell::new("ok")]);
        t.row(vec![Cell::new("wNEAR"), Cell::new("1,105.61")]);
        let plain = t.render(false);
        assert_eq!(plain, vec![
            "token     value  note",
            "SOL       13.60  ok",
            "wNEAR  1,105.61",
        ]);
        // Same layout once the escape bytes are stripped.
        let coloured = t.render(true);
        assert!(coloured[1].contains("\x1b[32m13.60\x1b[0m"));
        for (p, c) in plain.iter().zip(&coloured) {
            assert_eq!(visible_width(c), width(p));
        }
    }

    #[test]
    fn panel_frames_body_and_footer() {
        let lines = vec!["asset  value".to_string(), "SOL    13.60".to_string()];
        let s = render_panel("PORTFOLIO", "17:36:44", &lines, Some("total €13.60"), false);
        let rows: Vec<&str> = s.lines().collect();
        assert!(rows[0].starts_with("╭─ PORTFOLIO · 17:36:44 ─"));
        assert_eq!(rows[1], "│ asset  value");
        assert_eq!(rows[2], "│ SOL    13.60");
        assert_eq!(rows[3], "╰─ total €13.60");
        assert!(s.ends_with('\n'));
    }
}
