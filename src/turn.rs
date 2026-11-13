//! Runs an agent turn and renders its events in the terminal: the streamed Markdown reply, tool
//! lines, the approval prompt and the stats line.

use crate::markdown::Markdown;
use crate::ui;
use cortex_core::agent::{Agent, Approval, ApprovalRequest, Event, StopReason};

/// Send `text` and show the turn. `quiet` (for `cortex run -q`) shows only the answer, the
/// approval prompt and why a turn stopped.
pub fn send(agent: &mut Agent, text: &str, quiet: bool) -> Result<String, String> {
    if !quiet && !agent.model.is_empty() { println!("\n{}", ui::reply_header(&agent.model)); ui::track_start(); }
    let mut view = TurnView { model: agent.model.clone(), quiet, md: None, replies: 0 };
    agent.send(text, |e| view.show(e))
}

struct TurnView {
    model: String,
    quiet: bool,
    /// The reply being streamed in this step (None until its first text).
    md: Option<Markdown>,
    /// Model replies completed so far in this turn.
    replies: usize,
}

impl TurnView {
    fn show(&mut self, e: Event) {
        let quiet = self.quiet;
        match e {
            // which memories came with the message, so it's clear where an answer's facts came from
            Event::Recalled { notes } => if !quiet { ui::outln(&ui::faint(&format!("  memory: {}", notes.join(", ")))) },
            Event::Compressing => if !quiet { ui::outln(&ui::faint("  (context nearly full: compressing earlier conversation)")) },
            Event::PlainChatFallback(m) => if !quiet {
                ui::outln(&ui::faint(&format!("  ({}: {m}. Continuing as plain chat; pick a tool-capable model for agent features.)", self.model)));
            },
            Event::Text(t) => {
                let md = self.md.get_or_insert_with(|| {
                    // text right under the header; a blank line when it follows tool output
                    if !quiet && self.replies > 0 { ui::out("\n"); }
                    Markdown::for_terminal()
                });
                ui::out(&md.push(&ui::clean(&t)));
            }
            Event::ReplyEnd => {
                self.replies += 1;
                if self.finish_reply() { ui::out("\n"); } // end the reply's line
            }
            Event::ToolStarted { name, description } => if !quiet {
                ui::outln(&format!("  {} {}", tool_mark(), ui::muted(&ui::truncate(&label(&name, &description), 110))));
            },
            Event::ToolSkipped { name, description } => if !quiet {
                let l = format!("{}  (skipped: same as the previous step)", label(&name, &description));
                ui::outln(&format!("  {} {}", tool_mark(), ui::faint(&ui::truncate(&l, 110))));
            },
            Event::ApprovalRequest(r) => ask_approval(r),
            Event::ToolFinished { result } => if !quiet {
                let shown = ui::clean(&result);
                let lines: Vec<&str> = shown.lines().collect();
                for l in lines.iter().take(4) { ui::outln(&format!("    {}", ui::faint(&ui::truncate(l, 110)))); }
                if lines.len() > 4 { ui::outln(&format!("    {}", ui::faint(&format!("… {} more lines", lines.len() - 4)))); }
            },
            Event::Usage(_) => {}
            Event::Stopped(why) => ui::outln(&ui::faint(&match why {
                StopReason::Cancelled => "  [stopped]".to_string(),
                StopReason::Declined => "  (stopped: tell Cortex what you'd like instead)".to_string(),
                StopReason::Repeating => "  (stopped: the model kept repeating the same action)".to_string(),
                StopReason::StepLimit(n) => format!("  (stopped after {n} tool rounds; say \"continue\" to keep going)"),
            })),
            Event::Done { stats, .. } => if !quiet {
                let stats = ui::status_line(stats.ctx_used, stats.ctx_max, stats.tok_per_s, stats.tool_calls);
                // put the stats next to the model name; if that line has scrolled away, print them below
                if !ui::finish_header(&self.model, &stats) { println!("\n  {stats}"); }
            },
            // the caller prints the error; first finish whatever part of the reply was shown
            Event::Error(_) => { self.finish_reply(); }
        }
    }

    /// Print anything of the reply held back and reset styles. Returns whether there was a reply.
    fn finish_reply(&mut self) -> bool {
        let Some(mut md) = self.md.take() else { return false };
        ui::out(&md.finish());
        true
    }
}

/// "name: what it does", shown without control characters (both come from the model).
fn label(name: &str, description: &str) -> String { ui::clean(&format!("{name}: {description}")) }

fn tool_mark() -> String { ui::paint(ui::TOOL_MARK, ui::WHITE, true) }

/// Ask y / n / always on stdin.
fn ask_approval(r: ApprovalRequest) {
    if let Some(p) = &r.preview {
        for l in ui::clean(p).lines() { ui::outln(&format!("    {}", ui::faint(l))); }
    }
    let sep = ui::muted(" / ");
    ui::out(&format!("  {} {}{sep}{}{sep}{} ", ui::bold("allow?"),
        ui::paint("[y]es", ui::OK, false), ui::paint("[n]o", ui::ERR, false), ui::paint("[a]lways this session", ui::WARN, false)));
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() { return r.answer(Approval::Deny); }
    ui::feed(if line.ends_with('\n') { &line } else { "\n" }); // the user's typed answer + Enter
    r.answer(match line.trim().to_lowercase().as_str() {
        "y" | "yes" => Approval::AllowOnce,
        "a" | "always" => Approval::AllowAlways,
        _ => Approval::Deny,
    });
}
