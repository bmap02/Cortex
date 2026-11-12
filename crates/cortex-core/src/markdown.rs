//! Renders the model's Markdown for the terminal as it streams in: headings, **bold**,
//! *italics*, `inline code`, [links](url), bullet and numbered lists, quotes, rules and
//! syntax-highlighted code blocks.
//!
//! Text is handled a character at a time so replies still appear as they're generated. Only
//! the first few characters of a line (to tell a heading or list item from plain text), a lone
//! `*`, a possible link, and lines inside code blocks are held back briefly. Code lines get
//! colour only, never a prefix, so code copied from the terminal pastes cleanly. Without
//! colour (piped output, NO_COLOR) the text passes through untouched.

use crate::ui;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";
const CODE_BG: &str = "\x1b[48;2;48;48;48m";

// Loaded on the first code block, not at startup.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_nonewlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    ThemeSet::load_defaults().themes.remove("base16-ocean.dark").unwrap_or_default()
});

/// A highlighter for a code block's language tag ("rust", "py", "shell", ...); plain text if unknown.
fn highlighter(lang: &str) -> HighlightLines<'static> {
    let lang = lang.to_lowercase();
    let token = match lang.as_str() {
        "shell" | "console" | "terminal" | "zsh" => "bash",
        "ts" | "typescript" | "tsx" => "js", // closest bundled grammar
        "c++" => "cpp",
        other => other,
    };
    let syntaxes: &'static SyntaxSet = &SYNTAXES;
    let syntax = syntaxes.find_syntax_by_token(token).unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    HighlightLines::new(syntax, &THEME)
}

pub struct Markdown {
    color: bool,
    /// Nothing of the current line has been printed yet.
    at_start: bool,
    /// The start of the current line, until we know what kind of line it is.
    pending: String,
    /// `*` (1) or `**` (2) held back until the next character shows whether it styles text.
    stars: u8,
    /// A possible `[text](url)` link, held until we know whether it is one.
    link: Option<String>,
    /// Backticks that opened the code block we are in (0 outside one).
    fence: usize,
    /// Backticks that opened a ```markdown block we are in (0 outside one); its contents render as Markdown.
    md_fence: usize,
    /// Highlighter for the code block we're in (None outside one).
    code_hl: Option<HighlightLines<'static>>,
    heading: bool,
    quote: bool,
    bold: bool,
    italic: bool,
    strike: bool,
    /// A `~` held back to see whether a second one follows (`~~strike~~`).
    tilde: bool,
    /// Backticks seen in a row, not yet acted on.
    ticks: usize,
    /// Length of the backtick run that opened the inline code we're in (0 outside it).
    code_ticks: usize,
    /// The last character printed on this line.
    prev: char,
}

/// What a line turned out to be, with the byte length of its Markdown prefix.
enum Kind { Wait, Heading(usize), Bullet(usize), Numbered(usize), Quote(usize), Plain }

impl Markdown {
    pub fn new(color: bool) -> Markdown {
        Markdown {
            color, at_start: true, pending: String::new(), stars: 0, link: None, fence: 0, md_fence: 0,
            code_hl: None, heading: false, quote: false, bold: false, italic: false, strike: false, tilde: false, ticks: 0, code_ticks: 0, prev: ' ',
        }
    }

    /// Renders when stdout is a colour terminal, passes text through otherwise.
    pub fn for_terminal() -> Markdown { Markdown::new(ui::color_on()) }

    /// Feed the next piece of the reply; returns what to print now.
    pub fn push(&mut self, s: &str) -> String {
        if !self.color { return s.to_string(); }
        let mut out = String::new();
        for c in s.chars() { self.feed(c, &mut out); }
        out
    }

    /// The reply is complete: print anything held back and reset the styles.
    pub fn finish(&mut self) -> String {
        if !self.color { return String::new(); }
        let mut out = String::new();
        if !self.at_start {
            self.end_line(&mut out);
        } else if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.whole_line(&line, &mut out);
        }
        self.fence = 0;
        self.md_fence = 0;
        self.code_hl = None;
        out
    }

    fn feed(&mut self, c: char, out: &mut String) {
        if !self.at_start { return self.body(c, out); }
        if c == '\n' {
            let line = std::mem::take(&mut self.pending);
            self.whole_line(&line, out);
            out.push('\n');
            return;
        }
        self.pending.push(c);
        match self.classify(&self.pending) {
            Kind::Wait => {}
            kind => {
                let line = std::mem::take(&mut self.pending);
                self.start_line(kind, &line, out);
            }
        }
    }

    /// Decide what kind of line `p` (the start of a line) is, or that we need more of it.
    fn classify(&self, p: &str) -> Kind {
        if self.fence > 0 { return Kind::Wait; }
        let indent = p.len() - p.trim_start_matches(' ').len();
        let t = &p[indent..];
        let Some(first) = t.chars().next() else { return Kind::Wait };
        // maybe a stray reasoning tag (see whole_line)
        if first == '<' && ["<think>", "</think>"].iter().any(|tag| tag.starts_with(t)) { return Kind::Wait; }
        let run = t.chars().take_while(|&c| c == first).count();
        let next = t.chars().nth(run);
        match first {
            '`' if run >= 3 => Kind::Wait, // a code fence: wait for the whole line
            '`' | '-' | '*' | '_' | '+' | '#' | '>' if next.is_none() => Kind::Wait,
            '-' | '*' | '_' if run >= 3 => Kind::Wait, // maybe a rule (---)
            '-' | '*' | '+' if run == 1 && next == Some(' ') => Kind::Bullet(indent + 2),
            '#' if run <= 6 && next == Some(' ') => Kind::Heading(indent + run + 1),
            '>' if next == Some(' ') => Kind::Quote(indent + 2),
            '0'..='9' => {
                let digits = t.bytes().take_while(u8::is_ascii_digit).count();
                match &t[digits..] {
                    "" | "." => Kind::Wait,
                    rest if rest.starts_with(". ") => Kind::Numbered(indent + digits + 2),
                    _ => Kind::Plain,
                }
            }
            _ => Kind::Plain,
        }
    }

    /// Print a line's prefix in its rendered form, then the rest of what we have so far.
    fn start_line(&mut self, kind: Kind, line: &str, out: &mut String) {
        self.at_start = false;
        let rest = match kind {
            Kind::Heading(n) => { self.heading = true; self.restyle(out); &line[n..] }
            Kind::Bullet(n) => { out.push_str(&line[..n - 2]); out.push_str(&ui::muted("•")); out.push(' '); &line[n..] }
            Kind::Numbered(n) => { out.push_str(&ui::muted(&line[..n - 1])); out.push(' '); &line[n..] }
            Kind::Quote(n) => {
                out.push_str(&line[..n - 2]);
                out.push_str(&ui::faint("│"));
                out.push(' ');
                self.quote = true;
                self.restyle(out);
                &line[n..]
            }
            Kind::Wait | Kind::Plain => line,
        };
        for c in rest.chars() { self.body(c, out); }
    }

    /// A line we held back in full (code block lines, fences, rules, or a line that ended
    /// before we could classify it). Doesn't print the newline.
    fn whole_line(&mut self, line: &str, out: &mut String) {
        let t = line.trim();
        // the fences become a small label above the block and a corner below it
        let ticks = t.chars().take_while(|&c| c == '`').count();
        let bare = ticks >= 3 && ticks == t.len();
        if self.fence == 0 && self.md_fence > 0 && bare && ticks >= self.md_fence {
            self.md_fence = 0;
            out.push_str(&ui::faint("╰─"));
            return;
        }
        if self.fence == 0 && ticks >= 3 {
            let lang = t[ticks..].split_whitespace().next().unwrap_or("");
            out.push_str(&ui::faint(&if lang.is_empty() { "╭─".to_string() } else { format!("╭─ {lang}") }));
            // a Markdown sample is shown formatted, not as source (code blocks inside it still work)
            if self.md_fence == 0 && matches!(lang.to_lowercase().as_str(), "markdown" | "md") {
                self.md_fence = ticks;
            } else {
                self.fence = ticks;
                self.code_hl = Some(highlighter(lang));
            }
            return;
        }
        // only a bare fence at least as long as the opening one closes it, so a ```python
        // line inside a ````markdown sample stays part of the sample
        if self.fence > 0 && ticks >= self.fence && bare {
            self.fence = 0;
            self.code_hl = None;
            out.push_str(&ui::faint("╰─"));
            return;
        }
        if self.fence > 0 {
            // colour only: no prefix, so copied code pastes cleanly
            match self.code_hl.as_mut().and_then(|h| h.highlight_line(line, &SYNTAXES).ok()) {
                Some(ranges) => {
                    out.push_str(&syntect::util::as_24_bit_terminal_escaped(&ranges, false));
                    out.push_str(RESET);
                }
                None => out.push_str(&ui::paint(line, ui::TEXT, false)),
            }
            return;
        }
        // some models leak their reasoning tags into the reply; drop those lines
        if t == "<think>" || t == "</think>" { return; }
        let marks: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        if marks.len() >= 3 && ['-', '*', '_'].iter().any(|&m| marks.chars().all(|c| c == m)) {
            out.push_str(&ui::faint(&"─".repeat(40)));
            return;
        }
        let kind = match self.classify(line) {
            Kind::Wait => Kind::Plain,
            k => k,
        };
        self.start_line(kind, line, out);
        self.end_line(out);
    }

    fn body(&mut self, c: char, out: &mut String) {
        if let Some(mut buf) = self.link.take() {
            let mut consumed = false;
            if c != '\n' && buf.len() < 300 {
                buf.push(c);
                consumed = true;
                match link_progress(&buf) {
                    None => { self.link = Some(buf); return; }
                    Some(true) => { self.emit_link(&buf, out); return; }
                    Some(false) => {}
                }
            }
            // not a link after all ("[x]", "arr[0]"): print the `[` and replay the rest
            out.push('[');
            self.prev = '[';
            for ch in buf.chars().skip(1) { self.body(ch, out); }
            if consumed { return; }
        }
        if c != '`' { self.close_ticks(out); }
        if c == '\n' {
            self.end_line(out);
            out.push('\n');
            return;
        }
        if self.stars > 0 {
            let n = std::mem::take(&mut self.stars);
            if n == 1 && c == '*' {
                self.stars = 2;
                return;
            }
            // `*` / `**` only style text when they hug a word, so "2 * 3" and "x**2" stay as typed
            let on = if n == 2 { self.bold } else { self.italic };
            let opens = !on && !c.is_whitespace() && (self.prev.is_whitespace() || self.prev.is_ascii_punctuation());
            let closes = on && !self.prev.is_whitespace();
            if opens || closes {
                if n == 2 { self.bold = !on } else { self.italic = !on }
                self.restyle(out);
            } else {
                out.push_str(&"*".repeat(n.into()));
                self.prev = '*';
            }
        }
        if std::mem::take(&mut self.tilde) {
            if c == '~' {
                self.strike = !self.strike;
                self.restyle(out);
                return;
            }
            out.push('~'); // a single `~` is just text ("~/projects")
            self.prev = '~';
        }
        let code = self.code_ticks > 0;
        match c {
            '*' if !code => self.stars = 1,
            '~' if !code => self.tilde = true,
            '[' if !code => self.link = Some("[".into()),
            '`' => self.ticks += 1,
            _ => { out.push(c); self.prev = c; }
        }
    }

    /// A run of backticks just ended: it opens inline code, closes code opened by a run of the
    /// same length (so ``a ` b`` works), or is literal text inside code.
    fn close_ticks(&mut self, out: &mut String) {
        let n = std::mem::take(&mut self.ticks);
        if n == 0 { return; }
        if self.code_ticks == 0 {
            self.code_ticks = n;
        } else if n == self.code_ticks {
            self.code_ticks = 0;
        } else {
            out.push_str(&"`".repeat(n));
            return;
        }
        self.restyle(out);
    }

    /// `[text](url)`: underlined text, clickable in terminals that support OSC 8 links (Windows
    /// Terminal, VS Code, iTerm2, most Linux terminals), then the address dimmed (once, if it's
    /// the same as the text) so you can see where it goes before clicking.
    fn emit_link(&mut self, buf: &str, out: &mut String) {
        let close = buf.find("](").unwrap_or(buf.len());
        let text = &buf[1..close];
        // the address is the first word; an optional "title" after it is dropped
        let url = buf.get(close + 2..buf.len() - 1).unwrap_or("").split_whitespace().next().unwrap_or("");
        // the address comes from the model: never let control characters into an escape sequence
        let clickable = !url.is_empty() && !url.chars().any(char::is_control);
        if clickable { out.push_str(&format!("\x1b]8;;{url}\x1b\\")); }
        out.push_str(UNDERLINE);
        out.push_str(text);
        out.push_str(RESET);
        if clickable { out.push_str("\x1b]8;;\x1b\\"); }
        if !url.is_empty() && url != text { out.push_str(&ui::faint(&format!(" ({url})"))); }
        self.restyle(out);
        self.prev = ')';
    }

    /// End of a line: inline styles don't carry over to the next one.
    fn end_line(&mut self, out: &mut String) {
        if let Some(buf) = self.link.take() { out.push_str(&buf); }
        self.close_ticks(out);
        // `*` / `**` straight after a word closes its style even at the end of the line ("an *example*")
        let n = std::mem::take(&mut self.stars);
        let on = if n == 2 { self.bold } else { self.italic };
        if n > 0 && !(on && !self.prev.is_whitespace()) { out.push_str(&"*".repeat(n.into())); }
        if std::mem::take(&mut self.tilde) { out.push('~'); }
        if self.heading || self.quote || self.bold || self.italic || self.strike || self.code_ticks > 0 { out.push_str(RESET); }
        self.heading = false;
        self.quote = false;
        self.bold = false;
        self.italic = false;
        self.strike = false;
        self.code_ticks = 0;
        self.at_start = true;
        self.prev = ' ';
    }

    /// Switch the terminal to the current combination of styles.
    fn restyle(&self, out: &mut String) {
        out.push_str(RESET);
        if self.heading || self.bold { out.push_str(BOLD); }
        if self.italic { out.push_str(ITALIC); }
        if self.strike { out.push_str(STRIKE); }
        if self.code_ticks > 0 { out.push_str(CODE_BG); }
        let fg = if self.heading { Some(ui::WHITE) } else if self.quote { Some(ui::MUTED) } else { None };
        if let Some((r, g, b)) = fg { out.push_str(&format!("\x1b[38;2;{r};{g};{b}m")); }
    }
}

/// How far `buf` (starting with `[`) is from being a `[text](url)` link:
/// None = can't tell yet, Some(true) = complete link, Some(false) = not a link.
fn link_progress(buf: &str) -> Option<bool> {
    let close = buf.find(']')?;
    let after = &buf[close + 1..];
    if after.is_empty() { return None; }
    if close == 1 || !after.starts_with('(') { return Some(false); }
    if after.ends_with(')') { Some(true) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(s: &str) -> String {
        let mut md = Markdown::new(true);
        md.push(s) + &md.finish()
    }
    fn plain(s: &str) -> String {
        regex::Regex::new("\x1b\\[[0-9;]*m|\x1b\\]8;;[^\x1b]*\x1b\\\\").unwrap().replace_all(s, "").into_owned()
    }

    #[test]
    fn renders_blocks_and_inline_styles() {
        let out = render("**hi** there\n");
        assert_eq!(plain(&out), "hi there\n");
        assert!(out.contains(BOLD));
        assert_eq!(plain(&render("## Title\n")), "Title\n");
        assert_eq!(plain(&render("- item\n  * nested\n")), "• item\n  • nested\n");
        assert_eq!(plain(&render("1. one\n12. twelve\n")), "1. one\n12. twelve\n");
        assert_eq!(plain(&render("> quoted\n")), "│ quoted\n");
        assert_eq!(plain(&render("use `cargo test` and *really* do\n")), "use cargo test and really do\n");
        assert!(render("*really*").contains(ITALIC));
        assert_eq!(plain(&render("---\n")), format!("{}\n", "─".repeat(40)));
    }

    #[test]
    fn code_blocks_are_highlighted_and_copy_clean() {
        let out = render("```rust\nfn main() {\n    let x = 2 * 3; // six\n}\n```\n");
        // the code itself is exactly as written (no gutter), between a label and a corner
        assert_eq!(plain(&out), "╭─ rust\nfn main() {\n    let x = 2 * 3; // six\n}\n╰─\n");
        assert!(out.contains("\x1b[38;2;"), "rust code should be coloured");
        // unknown languages still render, uncoloured by a grammar
        assert_eq!(plain(&render("```nosuchlang\na_b *c*\n```\n")), "╭─ nosuchlang\na_b *c*\n╰─\n");
        // `sh` still finds the Bash grammar
        assert!(render("```sh\necho hi\n```\n").contains("\x1b[38;2;"));
    }

    #[test]
    fn markdown_blocks_render_formatted() {
        assert_eq!(plain(&render("```markdown\n# Title\n**bold** and [a link](https://x.io)\n- item\n```\nafter\n")),
            "╭─ markdown\nTitle\nbold and a link (https://x.io)\n• item\n╰─\nafter\n");
        // code blocks inside a Markdown sample, properly nested (````) or not (```), stay code
        let nested = "╭─ md\nTitle\n╭─ python\nprint(1)\n╰─\n╰─\nafter\n";
        assert_eq!(plain(&render("````md\n# Title\n```python\nprint(1)\n```\n````\nafter\n")), nested);
        assert_eq!(plain(&render("```md\n# Title\n```python\nprint(1)\n```\n```\nafter\n")), nested);
    }

    #[test]
    fn links() {
        assert_eq!(plain(&render("see [Google](https://google.com) now\n")), "see Google (https://google.com) now\n");
        let out = render("[Google](https://google.com)");
        assert!(out.contains(UNDERLINE));
        assert!(out.contains("\x1b]8;;https://google.com\x1b\\\x1b[4mGoogle\x1b[0m\x1b]8;;\x1b\\"), "clickable: {out:?}");
        assert_eq!(ui::width_of(&out), "Google (https://google.com)".len(), "link codes take no space");
        // an address with a control character is shown but not made clickable
        assert!(!render("[x](http://a.io/\x07y)").contains("\x1b]8;;"));
        assert_eq!(plain(&render("[https://a.io](https://a.io)\n")), "https://a.io\n");
        for s in ["- [x] done\n", "arr[0] = [1, 2]\n", "[not a link] (x)\n", "[unclosed\n"] {
            assert_eq!(plain(&render(s)), s.replace("- [x]", "• [x]"), "{s:?}");
        }
    }

    #[test]
    fn fixes_from_a_real_reply() {
        // italics that end the line, links with a title, double-backtick code, strikethrough
        assert_eq!(plain(&render("- item with *italics*\n")), "• item with italics\n");
        assert!(render("with *italics*\n").contains(ITALIC));
        assert_eq!(plain(&render("[a link](https://example.com \"Example\")\n")), "a link (https://example.com)\n");
        let out = render("``code with ` inside`` and `x`\n");
        assert_eq!(plain(&out), "code with ` inside and x\n");
        assert!(out.contains(CODE_BG));
        assert_eq!(plain(&render("~~gone~~ but ~/projects and ~one~\n")), "gone but ~/projects and ~one~\n");
        assert!(render("~~gone~~").contains(STRIKE));
        // stray reasoning tags some models emit are dropped; other tags are kept
        assert_eq!(plain(&render("answer\n</think>\n<b>bold</b>\n")), "answer\n\n<b>bold</b>\n");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for s in ["2 * 3 = 6\n", "x**2 + 1\n", "2 ** 3\n", "** note\n", "snake_case_name\n", "#hashtag\n", "1.5 apples\n", "-x\n", "a*b*c\n", "**\n"] {
            assert_eq!(plain(&render(s)), s, "{s:?}");
        }
    }

    #[test]
    fn streaming_in_pieces_matches_whole() {
        let text = "# Files\n\nThe **src/** folder has `main.rs`:\n- one\n- two\n```\ncode\n```\nDone";
        let mut md = Markdown::new(true);
        let mut out = String::new();
        for c in text.chars() { out += &md.push(&c.to_string()); }
        out += &md.finish();
        assert_eq!(plain(&out), plain(&render(text)));
        assert_eq!(plain(&out), "Files\n\nThe src/ folder has main.rs:\n• one\n• two\n╭─\ncode\n╰─\nDone");
    }

    #[test]
    fn passes_through_without_colour() {
        let mut md = Markdown::new(false);
        assert_eq!(md.push("**raw** markdown"), "**raw** markdown");
        assert_eq!(md.finish(), "");
    }
}
