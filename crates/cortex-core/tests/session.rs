//! Drives a core agent session through its public interface against the scripted fake model
//! server (tests/mock_server.py): start a turn, collect the events, answer approvals, cancel.

use cortex_core::agent::{Agent, Approval, Event, StopReason};
use cortex_core::config::Config;

mod common;
use common::{temp_home, MockServer};

fn agent(server: &MockServer) -> Agent {
    temp_home();
    let cfg = Config { base_url: format!("http://127.0.0.1:{}", server.port), model: "mock:7b".into(), ..Config::default() };
    Agent::new(cfg)
}

fn text_of(events: &[Event]) -> String {
    events.iter().filter_map(|e| match e { Event::Text(t) => Some(t.as_str()), _ => None }).collect()
}

/// The events of a turn as short labels, answering every approval request with `answer`.
fn run_turn(agent: &mut Agent, prompt: &str, answer: Approval) -> (Result<String, String>, Vec<String>) {
    let mut trace = vec![];
    let result = agent.send(prompt, |e| trace.push(match e {
        Event::ApprovalRequest(r) => { let l = format!("approval {}", r.name); r.answer(answer); l }
        Event::ToolStarted { name, .. } => format!("tool {name}"),
        Event::ToolFinished { result } => format!("result {result}"),
        Event::Stopped(why) => format!("stopped {why:?}"),
        Event::Done { .. } => "done".into(),
        Event::Error(e) => format!("error {e}"),
        _ => return,
    }));
    (result, trace)
}

/// A fresh, empty folder for files a tool writes.
fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cortex_{name}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn an_approved_tool_call_runs_and_the_model_answers_with_its_result() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    let file = scratch_dir("allowed").join("hello.txt");
    let (result, trace) = run_turn(&mut agent, &format!("write to {}", file.display()), Approval::AllowOnce);

    assert_eq!(trace[..3], ["tool write_file", "approval write_file", &format!("result wrote 18 bytes to {}", file.display())[..]]);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello from cortex\n");
    assert!(result.unwrap().starts_with("Done. The tool said: wrote 18 bytes"));
    assert_eq!(trace.last().unwrap(), "done");
}

#[test]
fn a_plain_reply_streams_text_then_finishes() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    let mut events = vec![];
    let answer = agent.send("hello", |e| events.push(e)).unwrap();

    assert_eq!(answer.trim(), "Hi! I'm a mock model running locally.");
    assert_eq!(text_of(&events).trim(), "Hi! I'm a mock model running locally.");
    assert!(!events.iter().any(|e| matches!(e, Event::ToolStarted { .. })));
    assert!(matches!(events.last(), Some(Event::Done { .. })), "a turn ends with Done");
}

#[test]
fn a_denied_tool_call_does_not_run_and_hands_control_back() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    let file = scratch_dir("denied").join("hello.txt");
    let (result, trace) = run_turn(&mut agent, &format!("write to {}", file.display()), Approval::Deny);

    assert!(!file.exists(), "the write didn't happen");
    assert_eq!(trace.len(), 5, "no further step after the denial: {trace:?}");
    assert_eq!(trace[..2], ["tool write_file", "approval write_file"]);
    assert!(trace[2].starts_with("result The user declined this action"));
    assert_eq!(trace[3..], ["stopped Declined", "done"]);
    assert_eq!(result.unwrap(), "", "the model didn't get to answer after the denial");
}

#[test]
fn cancelling_stops_the_reply_and_the_next_turn_runs_normally() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    let stop = agent.cancel.clone();
    let mut texts = 0;
    let mut stopped = false;
    let result = agent.send("tell me something slow", |e| match e {
        Event::Text(_) => { texts += 1; stop.cancel(); }
        Event::Stopped(StopReason::Cancelled) => stopped = true,
        _ => {}
    });

    assert!(stopped, "the turn reports that it was stopped");
    assert!(result.unwrap().split_whitespace().count() < 200, "the reply was cut short");
    assert!(texts < 200);
    // the stop belonged to that turn only
    let (result, trace) = run_turn(&mut agent, "hello", Approval::Deny);
    assert_eq!(result.unwrap().trim(), "Hi! I'm a mock model running locally.");
    assert_eq!(trace, ["done"]);
}

#[test]
fn stopping_while_an_approval_is_pending_ends_the_turn_without_running_the_tool() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    let file = scratch_dir("pending").join("hello.txt");
    let stop = agent.cancel.clone();
    let mut pending = vec![]; // a front-end that shows the request and waits for a click
    let mut trace = vec![];
    let result = agent.send(&format!("write to {}", file.display()), |e| match e {
        Event::ApprovalRequest(r) => {
            trace.push(format!("approval {}", r.description));
            pending.push(r);
            // the user presses Stop instead; it comes from another thread, like a GUI button
            let stop = stop.clone();
            std::thread::spawn(move || { std::thread::sleep(std::time::Duration::from_millis(50)); stop.cancel(); });
        }
        Event::Stopped(why) => trace.push(format!("stopped {why:?}")),
        Event::Done { .. } => trace.push("done".into()),
        _ => {}
    });

    assert!(result.is_ok());
    assert!(!file.exists(), "the write didn't happen");
    assert!(trace[0].starts_with(&format!("approval write {}", file.display())), "the request says what it does: {}", trace[0]);
    assert_eq!(trace[1..], ["stopped Cancelled", "done"]);
}

#[test]
fn a_turn_without_a_model_still_ends_with_an_error_event() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    agent.model.clear();
    let (result, trace) = run_turn(&mut agent, "hello", Approval::Deny);
    assert!(result.unwrap_err().contains("no model selected"));
    assert_eq!(trace.len(), 1);
    assert!(trace[0].starts_with("error no model selected"));
}

#[test]
fn the_turn_stops_at_the_step_limit() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    agent.cfg.max_steps = 1;
    let (result, trace) = run_turn(&mut agent, "what is 1234 * 5678?", Approval::Deny);
    assert!(result.is_ok());
    assert_eq!(trace, ["tool calculator", "result 7006652", "stopped StepLimit(1)", "done"]);
}

#[test]
fn a_nearly_full_context_is_compressed_before_the_next_reply() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    agent.cfg.num_ctx = 512;
    for i in 0..3 { // earlier conversation, far more than 512 tokens
        agent.msgs.push(cortex_core::backend::Msg::new("user", format!("question {i} {}", "padding ".repeat(200))));
        agent.msgs.push(cortex_core::backend::Msg::new("assistant", "an answer"));
    }
    let mut compressed = false;
    let answer = agent.send("hello", |e| if let Event::Compressing = e { compressed = true }).unwrap();
    assert!(compressed, "the turn reports that it compressed");
    assert_eq!(answer.trim(), "Hi! I'm a mock model running locally.", "and still answers the new message");
    assert!(agent.ctx_estimate() < 512, "the earlier conversation was replaced by a summary");
}

#[test]
fn remember_saves_a_note_with_its_session_and_new_sessions_know_it() {
    let server = MockServer::start();
    let home = scratch_dir("remember");
    let mut agent = agent(&server);
    agent.memory = cortex_core::memory::Memory::open(&home);
    let (result, trace) = run_turn(&mut agent, "please remember that the user's cat is called Biscuit", Approval::AllowOnce);
    assert!(result.is_ok());
    assert!(trace.contains(&"approval remember".to_string()), "remembering still asks first: {trace:?}");

    let note = agent.memory.notes().into_iter().find(|n| n.body.contains("Biscuit")).expect("a note on disk");
    assert_eq!(note.source.as_deref(), Some(agent.session_id.as_str()));
    let text = std::fs::read_to_string(&note.path).unwrap();
    assert!(text.starts_with("---\n") && text.contains("kind: fact"), "{text}");
    // a later session gets it back when it's relevant
    let mut later = self::agent(&server);
    later.memory = cortex_core::memory::Memory::open(&home);
    later.send("what's my cat called?", |_| {}).unwrap();
    assert!(sent_user_message(&server).contains("Biscuit"));
}

// ---------------------------------------------------------------- recall

/// An agent whose memory is its own folder with a few notes: the cat note matches "cat" and links
/// to the vet note; the coffee note is unrelated.
fn agent_with_pets(server: &MockServer, name: &str) -> Agent {
    use cortex_core::memory::{Memory, NewNote};
    let mut agent = agent(server);
    agent.memory = Memory::open(&scratch_dir(&format!("recall_{name}")));
    for (title, body) in [("Cat", "The cat is called Biscuit. Her vet is [[Vet]]."), ("Vet", "Dr Lee at the corner clinic."), ("Coffee", "Drinks black coffee in the morning.")] {
        agent.memory.add(NewNote { title: title.into(), body: body.into(), kind: "fact".into(), source: None }).unwrap();
    }
    agent
}

/// What the model was sent as the latest user message.
fn sent_user_message(server: &MockServer) -> String {
    let req: serde_json::Value = ureq::get(&format!("{}/debug/last-request", server.url())).call().unwrap().into_json().unwrap();
    req["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap().to_string()
}

#[test]
fn relevant_memories_and_their_linked_notes_come_with_the_message() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "inject");
    let mut recalled = vec![];
    agent.send("what is my cat called?", |e| if let Event::Recalled { notes } = e { recalled = notes }).unwrap();

    let sent = sent_user_message(&server);
    assert!(sent.contains("Biscuit"), "the matching note: {sent}");
    assert!(sent.contains("Dr Lee"), "the note it links to: {sent}");
    assert!(!sent.contains("coffee"), "not the unrelated note: {sent}");
    assert!(sent.trim_end().ends_with("what is my cat called?"), "the user's own words come last: {sent}");
    assert_eq!(recalled, ["Cat", "Vet"], "front-ends are told what was used");
    // the conversation keeps what the user typed; the memories were only for that request
    assert_eq!(agent.msgs.iter().rev().find(|m| m.role == "user").unwrap().content, "what is my cat called?");
    assert!(!agent.msgs[0].content.contains("Biscuit"), "the system prompt no longer lists every memory");
}

#[test]
fn recalled_memories_stay_within_the_budget() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "budget");
    agent.cfg.num_ctx = 512;
    // the whole memory section, markers included, has to fit: try budgets from too small to plenty
    for share in [0.01, 0.02, 0.03, 0.05, 0.2] {
        agent.cfg.memory_budget = share;
        agent.send("cat biscuit vet", |_| {}).unwrap();
        let sent = sent_user_message(&server);
        let block = sent.strip_suffix("cat biscuit vet").unwrap_or(&sent);
        let budget = (512.0 * share) as usize;
        assert!(block.len().div_ceil(4) <= budget, "budget {budget}, sent {} tokens: {block}", block.len().div_ceil(4));
    }
    assert!(sent_user_message(&server).contains("Biscuit"), "a big enough budget fits it all");
}

#[test]
fn with_no_memories_the_message_goes_as_typed() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    agent.memory = cortex_core::memory::Memory::open(&scratch_dir("recall_none"));
    agent.msgs[0].content = cortex_core::agent::system_prompt(&agent.cfg, &agent.memory);
    let mut recalled = false;
    agent.send("hello", |e| if let Event::Recalled { .. } = e { recalled = true }).unwrap();
    assert_eq!(sent_user_message(&server), "hello");
    assert!(!recalled);
    // nothing about memory for a user who has none: no recall tool, no memory line in the prompt
    let req: serde_json::Value = ureq::get(&format!("{}/debug/last-request", server.url())).call().unwrap().into_json().unwrap();
    let tools: Vec<&str> = req["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert!(!tools.contains(&"recall"), "{tools:?}");
    assert!(!req["messages"][0]["content"].as_str().unwrap().contains("[Memory]"));
    // and switching recall off sends it as typed too, memories or not
    let mut agent = agent_with_pets(&server, "off");
    agent.cfg.memory_budget = 0.0;
    agent.send("what is my cat called?", |_| {}).unwrap();
    assert_eq!(sent_user_message(&server), "what is my cat called?");
}

#[test]
fn the_recall_tool_returns_notes_with_their_links() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "tool");
    agent.cfg.memory_budget = 0.0; // only the tool, so its result is what we see
    let (_, trace) = run_turn(&mut agent, "recall cat", Approval::Deny);
    assert_eq!(trace[0], "tool recall", "no approval needed: it only reads");
    let result = &trace[1];
    assert!(result.contains("## Cat [[cat]]") && result.contains("Biscuit"), "{result}");
    assert!(result.contains("links to: vet"), "{result}");
}

#[test]
fn a_follow_up_question_still_brings_back_what_it_follows_up() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "follow_up");
    agent.send("what is my cat called?", |_| {}).unwrap();
    // nothing in this message matches a note; the conversation it continues does
    agent.send("and how old is she?", |_| {}).unwrap();
    let sent = sent_user_message(&server);
    assert!(sent.contains("Biscuit") && sent.contains("Dr Lee"), "{sent}");
}
