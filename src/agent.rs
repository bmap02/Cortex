//! The agent loop: send the conversation, stream the reply, run any tool calls,
//! feed results back, repeat until the model answers without tools.

use crate::backend::{Backend, BackendError, Msg, Usage, INTERRUPT};
use crate::config::{self, Config};
use crate::markdown::Markdown;
use crate::{tools, ui};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub model: String,
    pub created: u64,
    pub messages: Vec<Msg>, // without the system prompt (rebuilt on load)
}

pub fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

pub fn new_session_id() -> String {
    // yyyymmdd-hhmmss in UTC, no chrono dependency
    let t = now() as i64;
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

pub fn list_sessions() -> Vec<(String, String, usize)> {
    let mut v: Vec<(String, String, usize)> = std::fs::read_dir(config::sessions_dir()).into_iter().flatten().flatten()
        .filter_map(|e| {
            let s: Session = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            let first = s.messages.iter().find(|m| m.role == "user").map(|m| m.content.replace('\n', " ")).unwrap_or_default();
            Some((s.id, first, s.messages.len()))
        }).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v
}

/// Ids of saved sessions, newest first, from the file names (without reading the files).
pub fn session_ids() -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(config::sessions_dir()).into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(String::from))
        .collect();
    ids.sort_by(|a, b| b.cmp(a));
    ids
}

/// Delete saved sessions: all of them (None) or the one whose id starts with `prefix`.
pub fn delete_sessions(prefix: Option<&str>) -> Result<usize, String> {
    let ids: Vec<String> = session_ids().into_iter()
        .filter(|id| prefix.is_none_or(|p| id.starts_with(p))).collect();
    if let Some(p) = prefix {
        if ids.is_empty() { return Err(format!("no session matches '{p}'")); }
        if ids.len() > 1 { return Err(format!("'{p}' matches {} sessions; use more of the id", ids.len())); }
    }
    let mut n = 0;
    for id in ids {
        if std::fs::remove_file(config::sessions_dir().join(format!("{id}.json"))).is_ok() { n += 1; }
    }
    Ok(n)
}

/// The newest readable session whose id starts with `prefix` ("" = the latest one).
pub fn load_session(prefix: &str) -> Option<Session> {
    session_ids().into_iter().filter(|id| id.starts_with(prefix)).find_map(|id| {
        serde_json::from_str(&std::fs::read_to_string(config::sessions_dir().join(format!("{id}.json"))).ok()?).ok()
    })
}

pub struct Agent {
    pub cfg: Config,
    pub backend: Backend,
    pub model: String,
    pub msgs: Vec<Msg>, // msgs[0] is always the system prompt
    pub session_id: String,
    pub created: u64,
    pub approve_all: bool,
    pub tools_supported: bool,
    pub last: Usage,
    pub total_prompt: usize,
    pub total_completion: usize,
}

pub fn system_prompt(cfg: &Config) -> String {
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let mut s = format!(
"You are Cortex, a helpful assistant running entirely on the user's own machine. Nothing leaves this computer.
Today (UTC): {date}. OS: {os}. Shell for run_shell: {shell}. Working directory: {cwd}.

You can call tools to read and edit files, search code, run shell commands and Python, do exact math, and save memories.
- Use tools when they help; answer directly when they don't.
- Read a file before patching it, and keep edits minimal.
- Use the calculator for any arithmetic you'd otherwise estimate.
- Use `remember` only for durable facts the user would want kept (preferences, project facts), not chit-chat.
- Be concise. Use Markdown code blocks for code.{net}",
        date = &new_session_id()[..8], os = std::env::consts::OS, cwd = cwd, shell = tools::shell_name(),
        net = if cfg.allow_network { "" } else { "\n- You are offline: there is no internet access." });
    let folders: Vec<String> = ["Desktop", "Documents", "Downloads"].iter()
        .filter_map(|n| config::user_folder(n).map(|p| format!("{n} = {}", p.display()))).collect();
    if !folders.is_empty() { s += &format!("\nUser folders: {}.", folders.join(", ")); }
    if let Ok(soul) = std::fs::read_to_string(config::soul_path())
        && !soul.trim().is_empty() { s += &format!("\n\n# Persona\n{}", soul.trim()); }
    if let Ok(mem) = std::fs::read_to_string(config::memory_path())
        && !mem.trim().is_empty() { s += &format!("\n\n# What you remember about the user\n{}", mem.trim()); }
    s
}

impl Agent {
    pub fn new(cfg: Config) -> Agent {
        let backend = Backend::new(&cfg);
        Agent {
            model: cfg.model.clone(),
            msgs: vec![Msg::new("system", system_prompt(&cfg))],
            session_id: new_session_id(),
            created: now(),
            approve_all: cfg.auto_approve,
            tools_supported: true,
            last: Usage::default(),
            total_prompt: 0,
            total_completion: 0,
            backend,
            cfg,
        }
    }

    pub fn reset(&mut self) {
        self.msgs = vec![Msg::new("system", system_prompt(&self.cfg))];
        self.session_id = new_session_id();
        self.created = now();
        self.last = Usage::default();
    }

    pub fn resume(&mut self, s: Session) {
        self.msgs = vec![Msg::new("system", system_prompt(&self.cfg))];
        self.msgs.extend(s.messages);
        self.session_id = s.id;
        self.created = s.created;
    }

    pub fn save(&self) {
        if self.msgs.len() <= 1 { return; }
        let s = Session { id: self.session_id.clone(), model: self.model.clone(), created: self.created, messages: self.msgs[1..].to_vec() };
        let _ = std::fs::create_dir_all(config::sessions_dir());
        let _ = std::fs::write(config::sessions_dir().join(format!("{}.json", s.id)), serde_json::to_string_pretty(&s).unwrap_or_default());
    }

    /// Rough token estimate (chars / 4) when the server hasn't told us.
    pub fn ctx_estimate(&self) -> usize {
        self.msgs.iter().map(|m| m.content.len() + m.tool_calls.iter().map(|c| c.args.to_string().len()).sum::<usize>()).sum::<usize>() / 4
    }

    pub fn ctx_used(&self) -> usize {
        let reported = self.last.prompt_tokens + self.last.completion_tokens;
        if reported > 0 { reported } else { self.ctx_estimate() }
    }

    /// Ask a y/n/always question on stdin.
    fn approve(&mut self, preview: Option<String>) -> bool {
        if self.approve_all { return true; }
        if let Some(p) = preview {
            for l in p.lines() { ui::outln(&format!("    {}", ui::faint(l))); }
        }
        ui::out(&format!("  {} {} ", ui::bold("allow?"), ui::muted("[y]es / [n]o / [a]lways this session")));
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() { return false; }
        ui::feed(if line.ends_with('\n') { &line } else { "\n" }); // the user's typed answer + Enter
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" => true,
            "a" | "always" => { self.approve_all = true; true }
            _ => false,
        }
    }

    /// Run one user turn. `quiet` suppresses tool chatter (for `cortex run -q`).
    pub fn send(&mut self, text: &str, quiet: bool) -> Result<String, String> {
        if self.model.is_empty() {
            return Err("no model selected. Run `cortex setup` or `/model <name>`.".into());
        }
        self.msgs.push(Msg::new("user", text));
        self.run_loop(quiet)
    }

    fn run_loop(&mut self, quiet: bool) -> Result<String, String> {
        let mut tool_calls_made = 0;
        let mut final_text = String::new();
        if !quiet { println!("\n{}", ui::reply_header(&self.model)); ui::track_start(); }
        for step in 0..self.cfg.max_steps.max(1) {
            if self.ctx_used() > self.cfg.num_ctx * 85 / 100 && self.msgs.len() > 6 {
                if !quiet { ui::outln(&ui::faint("  (context nearly full: compressing earlier conversation)")); }
                self.compress()?;
            }
            let schemas = if self.tools_supported { tools::schemas(&self.cfg) } else { vec![] };
            let mut printed_any = false;
            let mut md = Markdown::for_terminal();
            let mut on_text = |t: &str| {
                // text right under the header; a blank line when it follows tool output
                if !printed_any { if !quiet && step > 0 { ui::out("\n"); } printed_any = true; }
                ui::out(&md.push(t));
            };
            let result = match self.backend.chat(&self.cfg, &self.model, &self.msgs, &schemas, &mut on_text) {
                Err(BackendError::ToolsUnsupported(m)) if self.tools_supported => {
                    self.tools_supported = false;
                    if !quiet { ui::outln(&ui::faint(&format!("  ({}: {m}. Continuing as plain chat; pick a tool-capable model for agent features.)", self.model))); }
                    self.backend.chat(&self.cfg, &self.model, &self.msgs, &[], &mut on_text)
                }
                other => other,
            };
            ui::out(&md.finish()); // print anything held back and reset styles, even after an error
            let reply = match result {
                Ok(r) => r,
                Err(e) => { self.msgs.pop_if_user(); return Err(e.to_string()); }
            };
            if !reply.content.is_empty() { ui::out("\n"); }
            self.last = reply.usage;
            self.total_prompt += reply.usage.prompt_tokens;
            self.total_completion += reply.usage.completion_tokens;

            let mut m = Msg::new("assistant", reply.content.clone());
            m.tool_calls = reply.tool_calls.clone();
            self.msgs.push(m);
            final_text = reply.content;

            if reply.interrupted {
                ui::outln(&ui::faint("  [stopped]"));
                // an interrupted tool request can't be answered; drop the calls
                if let Some(last) = self.msgs.last_mut() { last.tool_calls.clear(); }
                break;
            }
            if reply.tool_calls.is_empty() { break; }

            let mut declined = false;
            for call in &reply.tool_calls {
                if declined {
                    // every tool call needs a result; skip the rest after a "no"
                    self.msgs.push(Msg::tool_result(call, "Skipped: the user declined an earlier action in this step.".into()));
                    continue;
                }
                tool_calls_made += 1;
                let desc = tools::describe(&call.name, &call.args);
                if !quiet { ui::outln(&format!("  {} {}", ui::paint(ui::TOOL_MARK, ui::WHITE, true), ui::muted(&ui::truncate(&format!("{}: {desc}", call.name), 110)))); }
                let tool = tools::find(&call.name);
                let result = match &tool {
                    None => format!("ERROR: unknown tool '{}'", call.name),
                    Some(t) if !t.enabled(&self.cfg) => format!("ERROR: tool '{}' is disabled", call.name),
                    Some(t) if t.needs_approval && !self.approve(tools::preview(&call.name, &call.args)) => {
                        declined = true;
                        "The user declined this action. Do not retry it or try a workaround; wait for their instructions.".to_string()
                    }
                    Some(_) => tools::execute(&call.name, &call.args, &self.cfg),
                };
                if !quiet {
                    let lines: Vec<&str> = result.lines().collect();
                    for l in lines.iter().take(4) { ui::outln(&format!("    {}", ui::faint(&ui::truncate(l, 110)))); }
                    if lines.len() > 4 { ui::outln(&format!("    {}", ui::faint(&format!("… {} more lines", lines.len() - 4)))); }
                }
                self.msgs.push(Msg::tool_result(call, result));
                if INTERRUPT.load(Ordering::SeqCst) { break; }
            }
            if INTERRUPT.load(Ordering::SeqCst) { ui::outln(&ui::faint("  [stopped]")); break; }
            if declined {
                // hand control back to the user instead of letting the model try something else
                ui::outln(&ui::faint("  (stopped: tell Cortex what you'd like instead)"));
                break;
            }
            if step + 1 == self.cfg.max_steps {
                ui::outln(&ui::faint(&format!("  (stopped after {} tool rounds; say \"continue\" to keep going)", self.cfg.max_steps)));
            }
        }
        if !quiet {
            let stats = ui::status_line(self.ctx_used(), self.cfg.num_ctx, self.last.tok_per_s, tool_calls_made);
            // put the stats next to the model name; if that line has scrolled away, print them below
            if !ui::finish_header(&self.model, &stats) { println!("\n  {stats}"); }
        }
        self.save();
        Ok(final_text)
    }

    /// Replace the earlier conversation with a model-written summary. The latest user turn
    /// (and any tool calls after it) is kept as is, so a request in progress isn't lost.
    pub fn compress(&mut self) -> Result<(), String> {
        if self.msgs.len() <= 2 { return Ok(()); }
        let split = compress_split(&self.msgs);
        let mut req = self.msgs[..split].to_vec();
        req.push(Msg::new("user", "Summarize our conversation so far for your own future reference: the user's goals, key facts, decisions, files touched and anything still open. Be dense; bullet points."));
        let r = self.backend.chat(&self.cfg, &self.model, &req, &[], &mut |_| {}).map_err(|e| e.to_string())?;
        let mut tail = self.msgs.split_off(split);
        // Everything was summarized but a request is still being worked on (the last message is the
        // user's or a tool result): repeat the request so the model still has something to answer.
        if tail.is_empty() && self.msgs.last().is_some_and(|m| m.role != "assistant")
            && let Some(u) = self.msgs.iter().rfind(|m| m.role == "user") { tail.push(u.clone()); }
        let sys = self.msgs[0].clone();
        self.msgs = vec![sys, Msg::new("user", format!("[Summary of the earlier conversation]\n{}", r.content)), Msg::new("assistant", "Understood, I have the context.")];
        self.msgs.extend(tail);
        self.last = Usage::default();
        Ok(())
    }

    /// Remove the last user turn and everything after it; returns that user text.
    pub fn undo(&mut self) -> Option<String> {
        let i = self.msgs.iter().rposition(|m| m.role == "user")?;
        if i == 0 { return None; }
        let text = self.msgs[i].content.clone();
        self.msgs.truncate(i);
        Some(text)
    }
}

/// Index where compression splits the conversation: messages before the latest user turn are
/// summarized, the rest is kept. With no earlier turn to keep apart, everything is summarized.
fn compress_split(msgs: &[Msg]) -> usize {
    match msgs.iter().rposition(|m| m.role == "user") {
        Some(i) if i > 1 => i,
        _ => msgs.len(),
    }
}

trait PopIfUser { fn pop_if_user(&mut self); }
impl PopIfUser for Vec<Msg> {
    fn pop_if_user(&mut self) { if self.last().map(|m| m.role == "user").unwrap_or(false) { self.pop(); } }
}

#[cfg(test)]
mod tests {
    use super::{compress_split, Msg};

    #[test]
    fn compress_keeps_the_latest_turn() {
        let m = |role: &str| Msg::new(role, "x");
        // sys, old exchange, new user message + tool call + tool result: keep from the new user message
        let msgs = vec![m("system"), m("user"), m("assistant"), m("user"), m("assistant"), m("tool")];
        assert_eq!(compress_split(&msgs), 3);
        // only one user turn: nothing earlier to summarize separately, so summarize it all
        let msgs = vec![m("system"), m("user"), m("assistant"), m("tool")];
        assert_eq!(compress_split(&msgs), 4);
    }

    #[test]
    fn dates() {
        assert_eq!(super::civil_from_days(0), (1970, 1, 1));
        assert_eq!(super::civil_from_days(20397), (2025, 11, 5));
    }
}
