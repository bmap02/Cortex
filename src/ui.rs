//! Terminal styling (black & white theme), the startup banner and the status line.

use std::io::IsTerminal;
use std::sync::OnceLock;

type Rgb = (u8, u8, u8);
pub const WHITE: Rgb = (255, 255, 255);
pub const TEXT: Rgb = (232, 232, 232);
pub const MUTED: Rgb = (138, 138, 138);
pub const FAINT: Rgb = (94, 94, 94);
pub const BORDER: Rgb = (78, 78, 78);
pub const BAR_BG: Rgb = (51, 51, 51);
pub const ERR: Rgb = (255, 120, 120);
pub const OK: Rgb = (120, 220, 140);
pub const WARN: Rgb = (240, 190, 90);

pub fn color_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal()
            // Windows 10+: switch the console into ANSI mode; if that fails, print plain text
            && enable_ansi_support::enable_ansi_support().is_ok()
    })
}

/// Full banner unless the user opts out with CORTEX_ASCII=1. Every current terminal
/// (Windows Terminal, the Windows 11 console, VS Code, macOS/Linux terminals) can draw it.
pub fn rich_glyphs() -> bool {
    !matches!(std::env::var("CORTEX_ASCII").as_deref(), Ok(v) if !v.is_empty() && v != "0")
}

pub fn paint(s: &str, (r, g, b): Rgb, bold: bool) -> String {
    if !color_on() || s.is_empty() { return s.to_string(); }
    format!("\x1b[{}38;2;{r};{g};{b}m{s}\x1b[0m", if bold { "1;" } else { "" })
}
pub fn bold(s: &str) -> String { paint(s, WHITE, true) }
pub fn muted(s: &str) -> String { paint(s, MUTED, false) }
pub fn faint(s: &str) -> String { paint(s, FAINT, false) }
pub fn error(s: &str) -> String { paint(s, ERR, false) }

/// Columns a character takes on screen (2 for CJK and other wide characters).
fn char_width(c: char) -> usize { unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) }

/// Display width of plain text (no colour codes; see `width_of` for those).
pub fn width(s: &str) -> usize { s.chars().map(char_width).sum() }

/// Characters from `chars` while they fit in `budget` columns.
fn fit(chars: impl Iterator<Item = char>, budget: usize) -> Vec<char> {
    let mut used = 0;
    chars.take_while(|&c| { used += char_width(c); used <= budget }).collect()
}

pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max { return s.to_string(); }
    fit(s.chars(), max.saturating_sub(3)).into_iter().chain("...".chars()).collect()
}

/// Keep the end of a path: "…/projects/cortex".
pub fn truncate_start(s: &str, max: usize) -> String {
    if width(s) <= max { return s.to_string(); }
    let tail = fit(s.chars().rev(), max.saturating_sub(1));
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

/// "1 session", "3 sessions".
pub fn plural(n: usize, word: &str) -> String { format!("{n} {word}{}", if n == 1 { "" } else { "s" }) }

/// Where we are in an escape sequence: colours are CSI (`ESC [ ... m`), clickable links are
/// OSC 8 (`ESC ] 8;;url ESC \`).
#[derive(Clone, Copy)]
enum Esc { None, Start, Csi, Osc, OscEnd }

/// Whether `c` is part of an escape sequence (and so takes no space on screen); `esc` carries
/// the state between calls.
fn in_escape(esc: &mut Esc, c: char) -> bool {
    *esc = match (*esc, c) {
        (Esc::None, '\x1b') => Esc::Start,
        (Esc::None, _) => return false,
        (Esc::Start, '[') => Esc::Csi,
        (Esc::Start, ']') => Esc::Osc,
        (Esc::Csi, '@'..='~') => Esc::None,
        (Esc::Osc, '\x1b') => Esc::OscEnd,
        (Esc::Osc, '\x07') | (Esc::Start | Esc::OscEnd, _) => Esc::None,
        (state, _) => state,
    };
    true
}

pub fn bar(frac: f64, cells: usize) -> String {
    let filled = ((frac.clamp(0.0, 1.0) * cells as f64).round() as usize).min(cells);
    paint(&"█".repeat(filled), WHITE, false) + &paint(&"░".repeat(cells - filled), BAR_BG, false)
}

/// Real window width (Windows, macOS, Linux); falls back to $COLUMNS, then 100.
pub fn term_cols() -> usize {
    terminal_size::terminal_size().map(|(w, _)| w.0 as usize)
        .or_else(|| std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()))
        .unwrap_or(100)
}

/// Status-line bullet: ● exists in every Windows console font, ◉ doesn't.
pub const DOT: &str = "●";

/// Marks a tool call. Not ⚙: many terminals draw that as a 2-column emoji over the next character.
pub const TOOL_MARK: &str = "▸";

// ---------------------------------------------------------------- banner
const WORDMARK: [&str; 6] = [
    " ██████╗ ██████╗ ██████╗ ████████╗███████╗██╗  ██╗",
    "██╔════╝██╔═══██╗██╔══██╗╚══██╔══╝██╔════╝╚██╗██╔╝",
    "██║     ██║   ██║██████╔╝   ██║   █████╗   ╚███╔╝ ",
    "██║     ██║   ██║██╔══██╗   ██║   ██╔══╝   ██╔██╗ ",
    "╚██████╗╚██████╔╝██║  ██║   ██║   ███████╗██╔╝ ██╗",
    " ╚═════╝ ╚═════╝ ╚═╝  ╚═╝   ╚═╝   ╚══════╝╚═╝  ╚═╝",
];
const WORD_GRAD: [Rgb; 6] = [(255, 255, 255), (242, 242, 242), (220, 220, 220), (194, 194, 194), (164, 164, 164), (133, 133, 133)];

/// The Mesh symbol in braille dots (28 × 14 cells).
const MESH: [&str; 14] = [
    "             ⣀⣀             ",
    "            ⣸⡁⢈⣇            ",
    "        ⢀⣠⠶⠛⣹⢻⡟⣏⠛⠶⣄⡀        ",
    "   ⣠⠤⣤⡤⠞⠋⠁ ⣼⠃⢸⡇⠘⣧ ⠈⠙⠳⢦⣤⠤⣄   ",
    "   ⣇⣀⣼⣟⡛⠛⢛⡿⠛⠛⢻⡟⠛⠛⢿⡛⠛⢛⣻⣧⣀⣸   ",
    "    ⣿⠹⣎⠙⢳⣾⣁  ⢸⡇  ⣈⣷⡞⠋⣱⠏⣿    ",
    "    ⣿ ⠙⣦⡟ ⠉⠛⣶⠛⠛⣶⠛⠉ ⢻⣴⠋ ⣿    ",
    "    ⣿ ⣠⠟⣧ ⣀⣤⠿⣤⣤⠿⣤⣀ ⣼⠻⣄ ⣿    ",
    "    ⣿⣰⢏⣠⡼⢿⡉  ⢸⡇  ⢉⡿⢧⣄⡹⣆⣿    ",
    "   ⡏⠉⢻⣯⣥⣤⣬⣷⣤⣤⣼⣧⣤⣤⣾⣥⣤⣬⣽⡟⠉⢹   ",
    "   ⠙⠒⠛⠓⢦⣄⡀ ⢻⡄⢸⡇⢠⡟ ⢀⣠⡴⠞⠛⠒⠋   ",
    "        ⠈⠙⠶⣤⣹⣼⣧⣏⣤⠶⠋⠁        ",
    "            ⢹⡁⢈⡏            ",
    "             ⠉⠉             ",
];

pub struct Section { pub title: String, pub rows: Vec<(String, String)> }
pub struct Gauge { pub label: String, pub frac: f64, pub detail: String }

pub struct BannerInfo {
    pub version: String,
    pub model: String,
    pub backend: String,
    pub cwd: String,
    pub session: String,
    pub sections: Vec<Section>,
    pub gauges: Vec<Gauge>,
    pub footer: String,
}

struct Cell { text: String, w: usize }
fn cell(parts: &[(&str, Rgb, bool)]) -> Cell {
    Cell { text: parts.iter().map(|(s, c, b)| paint(s, *c, *b)).collect(), w: parts.iter().map(|(s, _, _)| width(s)).sum() }
}
fn blank() -> Cell { Cell { text: String::new(), w: 0 } }

pub fn banner(info: &BannerInfo) -> String {
    let cols = term_cols();
    if cols < 96 || !rich_glyphs() {
        return format!("{} {} {}\n",
            paint(&format!("{DOT} cortex"), WHITE, true), muted(&format!("v{}", info.version)),
            faint(&format!("· {} · {} · fully local", info.model, info.backend)));
    }
    let mut out = String::from("\n");
    for (line, c) in WORDMARK.iter().zip(WORD_GRAD) { out += &format!("  {}\n", paint(line, c, true)); }
    out.push('\n');

    const LW: usize = 30;
    let rw = cols.min(120).saturating_sub(LW + 4);

    let mut left = vec![blank()];
    for (i, l) in MESH.iter().enumerate() { left.push(cell(&[(l, if i < 9 { WHITE } else { (189, 189, 189) }, false)])); }
    left.push(blank());
    left.push(cell(&[("  ", TEXT, false), (&truncate(&info.model, LW - 4), WHITE, true)]));
    left.push(cell(&[("  ", TEXT, false), (&truncate(&info.backend, LW - 4), MUTED, false)]));
    left.push(cell(&[("  ", TEXT, false), (&truncate_start(&info.cwd, LW - 4), MUTED, false)]));
    left.push(cell(&[("  ", TEXT, false), (&truncate(&format!("session {}", info.session), LW - 4), FAINT, false)]));

    let mut right = vec![];
    for sec in &info.sections {
        right.push(cell(&[(&sec.title, WHITE, true)]));
        for (label, value) in &sec.rows {
            let lab = format!("{label}: ");
            let val = truncate(value, rw.saturating_sub(width(&lab) + 2));
            right.push(cell(&[(&lab, MUTED, false), (&val, TEXT, false)]));
        }
        right.push(blank());
    }
    if !info.gauges.is_empty() {
        right.push(cell(&[("System", WHITE, true)]));
        for g in &info.gauges {
            let det = truncate(&g.detail, rw.saturating_sub(5 + 18 + 3));
            right.push(Cell { text: muted(&format!("{:<5}", g.label)) + &bar(g.frac, 18) + " " + &paint(&det, TEXT, false), w: 5 + 18 + 1 + width(&det) });
        }
        right.push(blank());
    }
    right.push(cell(&[(&truncate(&info.footer, rw - 2), MUTED, false)]));

    let inner = LW + rw;
    let title = format!(" Cortex v{} · fully local · no telemetry ", info.version);
    let side = inner.saturating_sub(width(&title)) / 2;
    out += &format!("{}{}{}\n",
        paint(&format!("╭{}", "─".repeat(side)), BORDER, false), paint(&title, WHITE, true),
        paint(&format!("{}╮", "─".repeat(inner.saturating_sub(side + width(&title)))), BORDER, false));
    for i in 0..left.len().max(right.len()) {
        let e = blank();
        let (l, r) = (left.get(i).unwrap_or(&e), right.get(i).unwrap_or(&e));
        out += &format!("{}{}{}{}{}{}\n", paint("│ ", BORDER, false), l.text, " ".repeat(LW.saturating_sub(l.w)),
            r.text, " ".repeat(rw.saturating_sub(r.w).saturating_sub(1)), paint("│", BORDER, false));
    }
    out += &paint(&format!("╰{}╯", "─".repeat(inner)), BORDER, false);
    out += "\n\n";
    out += &paint("Welcome to Cortex. Everything runs on this machine. Type a message or /help.", TEXT, false);
    out += "\n";
    out += &faint("✦ Tip: end a line with \\ for multi-line input. Ctrl+C stops a reply.");
    out += "\n";
    out
}

// ---------------------------------------------------------------- reply output
// Everything printed during a reply goes through out()/outln() so we know how many screen
// rows it took. When the reply finishes we jump back up and add the stats to the model line.

struct Tracker { rows: usize, col: usize, width: usize, esc: Esc }
static TRACK: std::sync::Mutex<Tracker> = std::sync::Mutex::new(Tracker { rows: 0, col: 0, width: 100, esc: Esc::None });

/// Count how text advances the cursor (wrapping, newlines, wide chars, colour codes).
pub fn feed(s: &str) {
    let Ok(mut t) = TRACK.lock() else { return };
    for c in s.chars() {
        if in_escape(&mut t.esc, c) { continue; }
        match c {
            '\n' => { t.rows += 1; t.col = 0; }
            '\r' => t.col = 0,
            '\t' => { let next = (t.col / 8 + 1) * 8; t.col = next.min(t.width); }
            _ => {
                let w = char_width(c);
                if w == 0 { continue; }
                if t.col + w > t.width { t.rows += 1; t.col = 0; }
                t.col += w;
            }
        }
    }
}

pub fn out(s: &str) {
    print!("{s}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    feed(s);
}
pub fn outln(s: &str) { out(s); out("\n"); }

/// Start counting rows below the line just printed.
pub fn track_start() {
    if let Ok(mut t) = TRACK.lock() { *t = Tracker { rows: 0, col: 0, width: term_cols().max(20), esc: Esc::None }; }
}

/// Rewrite the model line at the top of the reply so it includes the stats.
/// Returns false when it can't (not a terminal, line scrolled off screen, too long).
pub fn finish_header(model: &str, stats: &str) -> bool {
    if !color_on() { return false; }
    if TRACK.lock().map(|t| t.col > 0).unwrap_or(false) { out("\n"); } // make sure we're at column 0
    let (rows, width) = match TRACK.lock() { Ok(t) => (t.rows, t.width), Err(_) => return false };
    let line = reply_header(model) + &paint(" │ ", BORDER, false) + stats;
    let height = terminal_size::terminal_size().map(|(_, h)| h.0 as usize).unwrap_or(0);
    let up = rows + 1; // the model line sits just above the first tracked row
    if height == 0 || up >= height || width_of(&line) >= width { return false; }
    print!("\x1b[{up}A\r{line}\x1b[K\x1b[{up}B\r");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    true
}

/// Visible width of a string that may contain colour codes.
pub fn width_of(s: &str) -> usize {
    let mut esc = Esc::None;
    s.chars().filter(|&c| !in_escape(&mut esc, c)).map(char_width).sum()
}

/// Shown above each reply so it's clear who's talking.
pub fn reply_header(model: &str) -> String { paint(&format!("{DOT} {model}"), WHITE, true) }

/// Stats under each reply.
pub fn status_line(ctx_used: usize, ctx_max: usize, tok_s: f64, steps: usize) -> String {
    let sep = paint(" │ ", BORDER, false);
    let k = |n: usize| if n >= 1024 {
        let v = n as f64 / 1024.0;
        if v.fract() < 0.05 { format!("{v:.0}k") } else { format!("{v:.1}k") }
    } else { n.to_string() };
    let frac = if ctx_max > 0 { ctx_used as f64 / ctx_max as f64 } else { 0.0 };
    let mut s = muted(&format!("ctx {}/{} ", k(ctx_used), k(ctx_max))) + &muted("[") + &bar(frac, 10) + &muted("]") + &sep
        + &muted(&format!("{tok_s:.0} tok/s"));
    if steps > 0 { s += &sep; s += &muted(&plural(steps, "tool call")); }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn widths_and_truncation() {
        assert_eq!(width("日本"), 4);
        assert_eq!(width_of("\x1b[1;38;2;255;255;255mhi\x1b[0m"), 2);
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("日本語のテキスト", 9), "日本語...");
        assert_eq!(truncate_start("/home/me/projects/cortex", 10), "…ts/cortex");
        assert_eq!(plural(1, "session"), "1 session");
        assert_eq!(plural(2, "session"), "2 sessions");
    }
}
