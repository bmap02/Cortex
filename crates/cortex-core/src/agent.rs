//! The agent loop: send the conversation, stream the reply, run any tool calls,
//! feed results back, repeat until the model answers without tools.

//!
//! A turn runs on a worker thread and reports what happens as [`Event`]s; the core itself never
//! prints or reads stdin. Front-ends render the events, answer [`ApprovalRequest`]s and stop a
//! turn through the session's [`CancelHandle`].

use crate::backend::{Backend, BackendError, Msg, Usage};
use crate::cancel::CancelHandle;
use crate::config::{self, Config};
use crate::memory::{self, Memory};
use crate::tools;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub model: String,
    pub created: u64,
    pub messages: Vec<Msg>, // without the system prompt (rebuilt on load)
    /// Memories the agent suggested that the user hasn't reviewed yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<memory::Candidate>,
}

pub fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }

pub fn new_session_id() -> String {
    // yyyymmdd-hhmmss in UTC, no chrono dependency
    let t = now() as i64;
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// An id for a new session that no saved session uses yet: `cortex run` started twice in the
/// same second would otherwise save over the first conversation.
/// shortcut: two Cortex windows starting in the same second can still pick the same id (neither
/// has saved yet); claim the file at creation if that ever matters.
fn fresh_session_id() -> String {
    first_free(&new_session_id(), |id| config::sessions_dir().join(format!("{id}.json")).exists())
}

/// `base`, or `base-2`, `base-3`, ... whichever isn't taken.
pub(crate) fn first_free(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) { return base.to_string(); }
    (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).expect("some suffix is free")
}

/// A UTC time as 2026-11-12T15:30:00Z.
pub fn utc_iso(unix_secs: u64) -> String {
    let t = unix_secs as i64;
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
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

/// Ids for what the user typed: just that id if it exists (so `…-848` doesn't also mean
/// `…-848-2`), otherwise every id starting with it, newest first.
fn matching(ids: Vec<String>, typed: &str) -> Vec<String> {
    if ids.iter().any(|id| id == typed) { return vec![typed.to_string()]; }
    ids.into_iter().filter(|id| id.starts_with(typed)).collect()
}

/// Delete saved sessions: all of them (None) or the one matching `prefix`.
pub fn delete_sessions(prefix: Option<&str>) -> Result<usize, String> {
    let ids = match prefix { Some(p) => matching(session_ids(), p), None => session_ids() };
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

/// The session with id `prefix`, else the newest readable one whose id starts with it ("" = the latest).
pub fn load_session(prefix: &str) -> Option<Session> {
    matching(session_ids(), prefix).into_iter().find_map(|id| {
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
    /// Stops the turn in progress; the front-end sets it (Ctrl-C, a Stop button).
    pub cancel: CancelHandle,
    /// Long-term memory: recalled into each message, and what `remember` and `recall` use.
    pub memory: Memory,
    /// The memories recalled for the turn in progress, as sent ahead of its message.
    turn_memory_block: Option<String>,
    /// Ids of the memory notes this session recalled or saved: its session note links to them.
    used: RefCell<BTreeSet<String>>,
    /// The conversation has moved on since its session note was last written.
    note_due: bool,
    /// Memories suggested in this session, waiting for the user's review.
    candidates: RefCell<Vec<memory::Candidate>>,
}

/// What happens during a turn, in order. The last event is always [`Event::Done`] or [`Event::Error`].
pub enum Event {
    /// The conversation is nearly as long as the context window: earlier turns are being summarized.
    Compressing,
    /// The model can't call tools (the server's message); the turn carries on as plain chat.
    PlainChatFallback(String),
    /// The next piece of the model's reply, as received (may contain control characters).
    Text(String),
    /// The model's reply for this step is complete, or was stopped. A reply that fails ends the
    /// turn with [`Event::Error`] instead.
    ReplyEnd,
    /// A tool call is starting. `description` says what it does, e.g. "1234 * 5678" for the calculator.
    ToolStarted { name: String, description: String },
    /// A tool call identical to one in the previous step: not run again.
    ToolSkipped { name: String, description: String },
    /// Notes from long-term memory sent with this message (their titles).
    Recalled { notes: Vec<String> },
    /// The agent suggested a memory; it waits for the user's review (see [`Agent::to_review`]).
    MemoryCandidate(memory::Candidate),
    /// A tool call needs the user's OK before it runs.
    ApprovalRequest(ApprovalRequest),
    /// The tool call that last started has finished with this result (what the model sees).
    ToolFinished { result: String },
    /// Token counts and speed for one model reply.
    Usage(Usage),
    /// The turn ended before the model finished answering.
    Stopped(StopReason),
    /// The turn is over; `text` is the model's last reply.
    Done { text: String, stats: TurnStats },
    /// The turn failed; the user's message was taken back out of the conversation.
    Error(String),
}

/// Why a turn stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The cancel handle was set.
    Cancelled,
    /// The user declined a tool call: control goes back to them.
    Declined,
    /// The model kept repeating the same tool call.
    Repeating,
    /// The model used up the maximum number of tool rounds for one message.
    StepLimit(usize),
}

/// The context and speed figures for the status line at the end of a turn.
#[derive(Debug, Clone, Copy)]
pub struct TurnStats {
    pub ctx_used: usize,
    pub ctx_max: usize,
    pub tok_per_s: f64,
    /// Tool calls that ran (repeats that were skipped don't count).
    pub tool_calls: usize,
}

/// A request to run a tool. Answer it with [`ApprovalRequest::answer`]; dropping it unanswered
/// counts as [`Approval::Deny`]. The turn waits until one of the two happens, or until it is
/// cancelled, which also denies it (a request kept after that is simply ignored).
pub struct ApprovalRequest {
    pub name: String,
    /// What the call does, as in [`Event::ToolStarted`].
    pub description: String,
    /// Exactly what will run or change, when the one-line description doesn't show all of it.
    pub preview: Option<String>,
    reply: Sender<Approval>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    AllowOnce,
    /// Allow this and every later tool call in this session without asking.
    AllowAlways,
    Deny,
}

impl ApprovalRequest {
    pub fn answer(self, a: Approval) { let _ = self.reply.send(a); }
}

/// The system prompt. `memory` decides whether it explains recalled memories: a user without any
/// gets the same prompt as before memory existed.
pub fn system_prompt(cfg: &Config, memory: &Memory) -> String {
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let mut s = format!(
"You are Cortex, a helpful assistant running entirely on the user's own machine. Nothing leaves this computer.
Today (UTC): {date}. OS: {os}. Shell for run_shell: {shell}. Working directory: {cwd}.

You can call tools to read and edit files, search code, run shell commands and Python, do exact math, and save memories.
- Use tools when they help; answer directly when they don't.
- Read a file before patching it, and keep edits minimal.
- Use the calculator for any arithmetic you'd otherwise estimate.
- When you learn a durable fact the user would want kept (a preference, a project fact), propose it with `suggest_memory`: the user reviews suggestions later. Use `remember` only when the user asks you to save something now. Never either for chit-chat.
- Be concise. Use Markdown code blocks for code.{net}",
        date = &new_session_id()[..8], os = std::env::consts::OS, cwd = cwd, shell = tools::shell_name(),
        net = if cfg.allow_network { "" } else { "\n- You are offline: there is no internet access." });
    let folders: Vec<String> = ["Desktop", "Documents", "Downloads"].iter()
        .filter_map(|n| config::user_folder(n).map(|p| format!("{n} = {}", p.display()))).collect();
    if !folders.is_empty() { s += &format!("\nUser folders: {}.", folders.join(", ")); }
    if !memory.is_empty() {
        s += "\n- Notes from your long-term memory may come before the user's message, between [Memory] and [End of memory]. Use what helps and ignore the rest; `recall` searches memory for more.";
    }
    if let Ok(soul) = std::fs::read_to_string(config::soul_path())
        && !soul.trim().is_empty() { s += &format!("\n\n# Persona\n{}", soul.trim()); }
    s
}

/// The memories recalled for a message, as they go ahead of it.
fn memory_block(notes: &[memory::Note]) -> String {
    format!("[Memory]\n{}\n[End of memory]\n\n", memory::render_recalled(notes))
}

impl Agent {
    pub fn new(cfg: Config) -> Agent {
        let backend = Backend::new(&cfg);
        let memory = Memory::open(&config::home());
        Agent {
            model: cfg.model.clone(),
            msgs: vec![Msg::new("system", system_prompt(&cfg, &memory))],
            session_id: fresh_session_id(),
            created: now(),
            approve_all: cfg.auto_approve,
            tools_supported: true,
            last: Usage::default(),
            total_prompt: 0,
            total_completion: 0,
            cancel: CancelHandle::new(),
            memory,
            turn_memory_block: None,
            used: RefCell::default(),
            note_due: false,
            candidates: RefCell::default(),
            backend,
            cfg,
        }
    }

    pub fn reset(&mut self) {
        self.msgs = vec![Msg::new("system", system_prompt(&self.cfg, &self.memory))];
        self.session_id = fresh_session_id();
        self.created = now();
        self.last = Usage::default();
        self.used.take();
        self.note_due = false;
        self.candidates.take();
    }

    pub fn resume(&mut self, s: Session) {
        self.candidates.replace(s.candidates);
        self.msgs = vec![Msg::new("system", system_prompt(&self.cfg, &self.memory))];
        self.msgs.extend(s.messages);
        self.session_id = s.id;
        self.created = s.created;
        // its note is rewritten whole when it ends: keep the links it has from before
        let linked = self.memory.find(&self.note_id()).map(|n| self.memory.links(&n).outgoing.into_iter().map(|l| l.target).collect());
        self.used.replace(linked.unwrap_or_default());
        self.note_due = false;
    }

    pub fn save(&self) {
        if self.msgs.len() <= 1 { return; }
        let s = Session { id: self.session_id.clone(), model: self.model.clone(), created: self.created, messages: self.msgs[1..].to_vec(),
            candidates: self.candidates.borrow().clone() };
        let _ = std::fs::create_dir_all(config::sessions_dir());
        // written whole, then swapped in: a crash while saving never leaves half a session
        let path = config::sessions_dir().join(format!("{}.json", s.id));
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_string_pretty(&s).unwrap_or_default()).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// The notes relevant to a new message, within the memory budget (the markers around them
    /// included). They're looked up by the message and the two before it, so a follow-up like
    /// "and when is her appointment?" still finds what it follows up.
    fn recall_for(&self, text: &str) -> Vec<memory::Note> {
        let budget = self.cfg.memory_tokens().saturating_sub(memory_block(&[]).len().div_ceil(4));
        if budget == 0 { return vec![]; }
        let mut query: Vec<&str> = self.msgs.iter().rev().filter(|m| m.role == "user").take(2).map(|m| m.content.as_str()).collect();
        query.push(text);
        // not this session's own note: its summary is in the conversation already
        let own = self.note_id();
        // the backend re-ranks by meaning when the server has an embedding model
        let mut notes = self.memory.recall(&query.join("\n"), 8, budget, Some(&self.backend));
        notes.retain(|n| n.id != own);
        notes
    }

    /// The conversation as sent to the model: the recalled memories go ahead of the latest user
    /// message in this request only. They aren't kept in the conversation, so saved sessions and
    /// /undo see what the user typed, and the system prompt stays the same from turn to turn
    /// (rewriting it would make the server re-read the whole conversation every time).
    fn request(&self) -> Vec<Msg> {
        let mut msgs = self.msgs.clone();
        if let (Some(block), Some(i)) = (&self.turn_memory_block, msgs.iter().rposition(|m| m.role == "user")) {
            msgs[i].content = format!("{block}{}", msgs[i].content);
        }
        msgs
    }

    /// Rough token estimate (chars / 4) when the server hasn't told us.
    pub fn ctx_estimate(&self) -> usize {
        self.msgs.iter().map(|m| m.content.len() + m.tool_calls.iter().map(|c| c.args.to_string().len()).sum::<usize>()).sum::<usize>() / 4
    }

    pub fn ctx_used(&self) -> usize {
        let reported = self.last.prompt_tokens + self.last.completion_tokens;
        if reported > 0 { reported } else { self.ctx_estimate() }
    }

    /// Wait for the front-end's answer to a tool call that needs approval.
    fn approve(&mut self, name: &str, description: &str, preview: Option<String>, emit: &impl Fn(Event)) -> bool {
        if self.approve_all { return true; }
        let (reply, answer) = mpsc::channel();
        emit(Event::ApprovalRequest(ApprovalRequest { name: name.into(), description: description.into(), preview, reply }));
        loop {
            match answer.recv_timeout(Duration::from_millis(100)) {
                Ok(Approval::AllowOnce) => return true,
                Ok(Approval::AllowAlways) => { self.approve_all = true; return true }
                Ok(Approval::Deny) | Err(RecvTimeoutError::Disconnected) => return false,
                // a Stop pressed while a dialog is still open
                Err(RecvTimeoutError::Timeout) if self.cancel.is_cancelled() => return false,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Run one user turn on a worker thread. `on_event` gets every [`Event`] on the calling thread
    /// as it happens and must answer (or drop) each [`ApprovalRequest`]. Returns the model's last
    /// reply, or the error that ended the turn.
    pub fn send(&mut self, text: &str, mut on_event: impl FnMut(Event)) -> Result<String, String> {
        if self.model.is_empty() {
            let e = "no model selected. Run `cortex setup` or `/model <name>`.".to_string();
            on_event(Event::Error(e.clone()));
            return Err(e);
        }
        self.cancel.reset(); // a stop meant for an earlier turn doesn't carry over
        let notes = self.recall_for(text);
        self.used.borrow_mut().extend(notes.iter().map(|n| n.id.clone()));
        self.turn_memory_block = (!notes.is_empty()).then(|| memory_block(&notes));
        if !notes.is_empty() { on_event(Event::Recalled { notes: notes.into_iter().map(|n| n.title).collect() }); }
        self.msgs.push(Msg::new("user", text));
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                let emit = |e: Event| { let _ = tx.send(e); };
                let result = self.run_loop(&emit);
                emit(match &result {
                    Ok((text, stats)) => Event::Done { text: text.clone(), stats: *stats },
                    Err(e) => Event::Error(e.clone()),
                });
                result.map(|(text, _)| text)
            });
            for e in rx { on_event(e); }
            worker.join().unwrap_or_else(|_| Err("the agent stopped unexpectedly".into()))
        })
    }

    fn run_loop(&mut self, emit: &impl Fn(Event)) -> Result<(String, TurnStats), String> {
        let mut tool_calls_made = 0;
        let mut final_text = String::new();
        // Small models sometimes repeat the exact call they just made instead of using its result.
        // An identical call right after itself isn't run again (re-running tests after an edit is
        // still fine: the edit comes in between); two such steps in a row end the turn.
        let mut prev_calls: Vec<(String, String)> = vec![];
        let mut repeat_steps = 0;
        for step in 0..self.cfg.max_steps.max(1) {
            if self.ctx_used() > self.cfg.num_ctx * 85 / 100 && self.msgs.len() > 6 {
                emit(Event::Compressing);
                if let Err(e) = self.compress() { self.msgs.pop_if_user(); return Err(e); }
            }
            let mut schemas = if self.tools_supported { tools::schemas(&self.cfg) } else { vec![] };
            // nothing to recall without memories: don't offer the tool
            if self.memory.is_empty() { schemas.retain(|s| s["function"]["name"] != "recall"); }
            let mut on_text = |t: &str| emit(Event::Text(t.to_string()));
            let request = self.request();
            let result = match self.backend.chat(&self.cfg, &self.model, &request, &schemas, &self.cancel, &mut on_text) {
                Err(BackendError::ToolsUnsupported(m)) if self.tools_supported => {
                    self.tools_supported = false;
                    emit(Event::PlainChatFallback(m));
                    self.backend.chat(&self.cfg, &self.model, &request, &[], &self.cancel, &mut on_text)
                }
                other => other,
            };
            let reply = match result {
                Ok(r) => r,
                Err(e) => { self.msgs.pop_if_user(); return Err(e.to_string()); }
            };
            emit(Event::ReplyEnd);
            emit(Event::Usage(reply.usage));
            // a stopped reply never gets its token counts; keep the last real ones for the stats line
            if reply.usage.prompt_tokens + reply.usage.completion_tokens > 0 { self.last = reply.usage; }
            self.total_prompt += reply.usage.prompt_tokens;
            self.total_completion += reply.usage.completion_tokens;

            let mut m = Msg::new("assistant", reply.content.clone());
            m.tool_calls = reply.tool_calls.clone();
            self.msgs.push(m);
            final_text = reply.content;

            if reply.interrupted {
                emit(Event::Stopped(StopReason::Cancelled));
                // an interrupted tool request can't be answered; drop the calls
                if let Some(last) = self.msgs.last_mut() { last.tool_calls.clear(); }
                break;
            }
            if reply.tool_calls.is_empty() { break; }

            let mut declined = false;
            let this_step: Vec<(String, String)> = reply.tool_calls.iter().map(|c| (c.name.clone(), c.args.to_string())).collect();
            let mut only_repeats = true;
            for call in &reply.tool_calls {
                if declined {
                    // every tool call needs a result; skip the rest after a "no"
                    self.msgs.push(Msg::tool_result(call, "Skipped: the user declined an earlier action in this step.".into()));
                    continue;
                }
                if self.cancel.is_cancelled() {
                    // stopped mid-step: the calls not run yet still need a result
                    self.msgs.push(Msg::tool_result(call, "Skipped: the user stopped this step.".into()));
                    continue;
                }
                let description = tools::describe(&call.name, &call.args);
                if prev_calls.contains(&(call.name.clone(), call.args.to_string())) {
                    emit(Event::ToolSkipped { name: call.name.clone(), description });
                    self.msgs.push(Msg::tool_result(call, REPEAT_NOTE.into()));
                    continue;
                }
                only_repeats = false;
                tool_calls_made += 1;
                emit(Event::ToolStarted { name: call.name.clone(), description: description.clone() });
                let tool = tools::find(&call.name);
                let suggested = self.candidates.borrow().len();
                let result = match &tool {
                    None => format!("ERROR: unknown tool '{}'", call.name),
                    Some(t) if !t.enabled(&self.cfg) => format!("ERROR: tool '{}' is disabled", call.name),
                    Some(t) if t.needs_approval && !self.approve(&call.name, &description, tools::preview(&call.name, &call.args), emit) => {
                        declined = true;
                        "The user declined this action. Do not retry it or try a workaround; wait for their instructions.".to_string()
                    }
                    Some(_) => tools::execute(&call.name, &call.args, &tools::ToolContext { cfg: &self.cfg, cancel: &self.cancel, session_id: &self.session_id, memory: &self.memory, used: &self.used, candidates: &self.candidates, embedder: &self.backend }),
                };
                if let Some(c) = self.candidates.borrow().get(suggested) { emit(Event::MemoryCandidate(c.clone())); }
                emit(Event::ToolFinished { result: result.clone() });
                self.msgs.push(Msg::tool_result(call, result));
            }
            if self.cancel.is_cancelled() { emit(Event::Stopped(StopReason::Cancelled)); break; }
            if declined {
                // hand control back to the user instead of letting the model try something else
                emit(Event::Stopped(StopReason::Declined));
                break;
            }
            prev_calls = this_step;
            repeat_steps = if only_repeats { repeat_steps + 1 } else { 0 };
            if repeat_steps >= 2 {
                emit(Event::Stopped(StopReason::Repeating));
                break;
            }
            if step + 1 == self.cfg.max_steps {
                emit(Event::Stopped(StopReason::StepLimit(self.cfg.max_steps)));
            }
        }
        self.save();
        self.note_due = true;
        let stats = TurnStats { ctx_used: self.ctx_used(), ctx_max: self.cfg.num_ctx, tok_per_s: self.last.tok_per_s, tool_calls: tool_calls_made };
        Ok((final_text, stats))
    }

    /// Replace the earlier conversation with a model-written summary. The latest user turn
    /// (and any tool calls after it) is kept as is, so a request in progress isn't lost.
    pub fn compress(&mut self) -> Result<(), String> {
        if self.msgs.len() <= 2 { return Ok(()); }
        let split = compress_split(&self.msgs);
        let summary = self.summarize(&self.msgs[..split])?.ok_or("stopped while compressing the conversation; nothing was changed")?;
        // a note that can't be written now is written again when the session ends
        if self.user_turns() >= 2 { let _ = self.write_session_note(&summary); }
        let mut tail = self.msgs.split_off(split);
        // Everything was summarized but a request is still being worked on (the last message is the
        // user's or a tool result): repeat the request so the model still has something to answer.
        if tail.is_empty() && self.msgs.last().is_some_and(|m| m.role != "assistant")
            && let Some(u) = self.msgs.iter().rfind(|m| m.role == "user") { tail.push(u.clone()); }
        let sys = self.msgs[0].clone();
        self.msgs = vec![sys, Msg::new("user", format!("{SUMMARY_MARK}\n{summary}")), Msg::new("assistant", "Understood, I have the context.")];
        self.msgs.extend(tail);
        self.last = Usage::default();
        Ok(())
    }

    /// A model-written summary of `msgs`, or None if it was stopped (a summary cut short would
    /// stand in for the whole conversation).
    fn summarize(&self, msgs: &[Msg]) -> Result<Option<String>, String> {
        let mut req = msgs.to_vec();
        // tests/mock_server.py recognizes this prompt: keep the two in step
        req.push(Msg::new("user", "Summarize our conversation so far for your own future reference: the user's goals, key facts, decisions, files touched and anything still open. Be dense; bullet points."));
        let r = self.backend.chat(&self.cfg, &self.model, &req, &[], &self.cancel, &mut |_| {}).map_err(|e| e.to_string())?;
        Ok((!r.interrupted).then_some(r.content))
    }

    /// The id of this session's note in long-term memory.
    fn note_id(&self) -> String { format!("session-{}", self.session_id) }

    /// Whether ending the session now would write its session note: there's more than a single
    /// question in it, and something new since the note was last written.
    pub fn wants_session_note(&self) -> bool {
        self.note_due && !self.model.is_empty() && self.user_turns() >= 2
    }

    fn user_turns(&self) -> usize { self.msgs.iter().filter(|m| m.role == "user").count() }

    /// Save the session's summary as its note in long-term memory, linked to the notes it used.
    /// Writes nothing (None) for a session too short to need one. Call it when the session ends;
    /// it asks the model for the summary, and the cancel handle stops that.
    pub fn end_session(&mut self) -> Result<Option<memory::Note>, String> {
        if !self.wants_session_note() { return Ok(None); }
        self.cancel.reset(); // a stop meant for the last turn doesn't carry over
        let summary = self.summarize(&self.msgs)?.ok_or("stopped while summarizing the session; no note was written")?;
        self.write_session_note(&summary).map(Some)
    }

    fn write_session_note(&mut self, summary: &str) -> Result<memory::Note, String> {
        let id = self.note_id();
        let all = self.memory.notes();
        // the title comes from where the session started, and stays when the note is rewritten
        let title = all.iter().find(|n| n.id == id).map(|n| n.title.clone()).unwrap_or_else(|| {
            let first = self.msgs.iter().find(|m| m.role == "user" && !m.content.starts_with(SUMMARY_MARK)).map_or("", |m| m.content.as_str());
            format!("Session: {}", memory::short_title(first))
        });
        // only notes still there: one deleted during the session would be a dead link
        let links: Vec<String> = self.used.borrow().iter().filter(|u| **u != id && all.iter().any(|n| n.id == **u)).map(|u| format!("[[{u}]]")).collect();
        let mut body = summary.trim().to_string();
        if !links.is_empty() { body += &format!("\n\nNotes used in this session: {}", links.join(" ")); }
        let note = self.memory.put(&id, memory::NewNote { title, body, kind: "session".into(), source: Some(self.session_id.clone()) })?;
        self.note_due = false;
        Ok(note)
    }

    /// What waits for the user's review: the memories suggested in this session and, when
    /// `consolidate`, ways to tidy the whole memory (see [`Memory::consolidation`]). Answer each with
    /// [`Agent::accept`] or [`Agent::reject`]; whatever isn't answered stays for later.
    pub fn to_review(&self, consolidate: bool) -> Vec<memory::Proposal> {
        let mut items: Vec<memory::Proposal> = self.candidates.borrow().iter().cloned().map(memory::Proposal::New).collect();
        if consolidate { items.extend(self.memory.consolidation()); }
        items
    }

    /// Carry out a reviewed proposal, with its text replaced by `edit` if the user changed it.
    /// A saved candidate is linked from this session's note. Returns the note created or changed.
    pub fn accept(&mut self, p: &memory::Proposal, edit: Option<&str>) -> Result<memory::Note, String> {
        let edited = edit.map(|t| p.with_text(t));
        let note = self.memory.apply(edited.as_ref().unwrap_or(p), Some(&self.session_id))?;
        if let memory::Proposal::New(_) = p { self.used.borrow_mut().insert(note.id.clone()); }
        self.drop_candidate(p);
        Ok(note)
    }

    /// Turn a proposal down: a suggested memory is dropped for good, a tidy-up just isn't done.
    pub fn reject(&mut self, p: &memory::Proposal) { self.drop_candidate(p); }

    fn drop_candidate(&mut self, p: &memory::Proposal) {
        let memory::Proposal::New(c) = p else { return };
        let before = self.candidates.borrow().len();
        self.candidates.borrow_mut().retain(|x| x != c);
        if self.candidates.borrow().len() != before { self.save(); } // the session file holds the rest
    }

    /// Remove the last user turn and everything after it; returns that user text.
    pub fn undo(&mut self) -> Option<String> {
        let i = self.msgs.iter().rposition(|m| m.role == "user")?;
        if i == 0 { return None; }
        let text = self.msgs[i].content.clone();
        // memories suggested in the removed turn go with it (results follow their calls in order)
        let removed = &self.msgs[i..];
        let mut undone = vec![];
        for (k, m) in removed.iter().enumerate() {
            for (j, c) in m.tool_calls.iter().enumerate() {
                let held = removed.get(k + 1 + j).is_some_and(|r| r.role == "tool" && r.content == tools::SUGGESTED);
                if c.name == "suggest_memory" && held && let Some(f) = c.args["fact"].as_str() { undone.push(memory::one_line(f)); }
            }
        }
        if !undone.is_empty() { self.candidates.borrow_mut().retain(|c| !undone.iter().any(|f| f.eq_ignore_ascii_case(&c.body))); }
        self.msgs.truncate(i);
        Some(text)
    }
}

/// How a compressed conversation's summary starts.
const SUMMARY_MARK: &str = "[Summary of the earlier conversation]";

/// Sent back instead of running a tool call identical to the one in the previous step.
const REPEAT_NOTE: &str = "Not run again: this is exactly the same call as your previous step, and its result is above. \
Use that result to continue, or answer the user now.";

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
    fn new_sessions_skip_taken_ids() {
        let taken = ["20261109-085848", "20261109-085848-2"];
        assert_eq!(super::first_free("20261109-090000", |id| taken.contains(&id)), "20261109-090000");
        assert_eq!(super::first_free("20261109-085848", |id| taken.contains(&id)), "20261109-085848-3");
    }

    #[test]
    fn full_ids_match_exactly() {
        let ids = || vec!["20261109-085848-2".to_string(), "20261109-085848".to_string(), "20261108-120000".to_string()];
        assert_eq!(super::matching(ids(), "20261109-085848"), ["20261109-085848"]);
        assert_eq!(super::matching(ids(), "20261109"), ["20261109-085848-2", "20261109-085848"]);
        assert_eq!(super::matching(ids(), "").len(), 3); // "" = all, newest first
    }

    #[test]
    fn dates() {
        assert_eq!(super::civil_from_days(0), (1970, 1, 1));
        assert_eq!(super::civil_from_days(20397), (2025, 11, 5));
    }
}
