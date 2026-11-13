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
    agent.memory = cortex_core::memory::Memory::open(&scratch_dir("compress_auto")); // it writes a session note
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

// ---------------------------------------------------------------- session notes

fn session_notes(agent: &Agent) -> Vec<cortex_core::memory::Note> {
    agent.memory.notes().into_iter().filter(|n| n.kind == "session").collect()
}

#[test]
fn ending_a_session_writes_a_session_note_that_later_sessions_find() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "end");
    agent.send("what is my cat called?", |_| {}).unwrap();
    agent.send("and how old is she?", |_| {}).unwrap();
    assert!(agent.wants_session_note());
    let note = agent.end_session().unwrap().expect("a session note");

    assert_eq!(note.id, format!("session-{}", agent.session_id));
    assert_eq!((note.kind.as_str(), note.source.as_deref()), ("session", Some(agent.session_id.as_str())));
    assert!(note.title.starts_with("Session: ") && note.title.contains("cat"), "{}", note.title);
    assert!(note.body.contains("Summary: the user asked what is my cat called? | and how old is she?"), "the model's summary: {}", note.body);
    assert!(note.body.contains("[[cat]]") && note.body.contains("[[vet]]"), "links to the notes it recalled: {}", note.body);
    assert!(!note.body.contains("[[coffee]]"), "not to notes it didn't use: {}", note.body);
    assert_eq!(agent.memory.links(&note).outgoing.len(), 2);

    // found by search, by the recall tool and by automatic recall
    assert!(agent.memory.search("how old", 5).iter().any(|h| h.note.id == note.id));
    let mut later = self::agent(&server);
    later.memory = cortex_core::memory::Memory::open(agent.memory.dir().parent().unwrap());
    later.cfg.memory_budget = 0.0;
    let (_, trace) = run_turn(&mut later, "recall how old", Approval::Deny);
    assert!(trace[1].contains(&format!("[[{}]]", note.id)), "{}", trace[1]);
    later.cfg.memory_budget = 0.5;
    later.send("how old is she again?", |_| {}).unwrap();
    assert!(sent_user_message(&server).contains(&format!("[[{}]]", note.id)), "{}", sent_user_message(&server));
}

#[test]
fn ending_again_after_more_messages_updates_the_same_note() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "update");
    agent.send("what is my cat called?", |_| {}).unwrap();
    agent.send("hello", |_| {}).unwrap();
    let first = agent.end_session().unwrap().expect("a session note");
    assert!(!agent.wants_session_note() && agent.end_session().unwrap().is_none(), "nothing new to note");

    agent.send("tell me about coffee", |_| {}).unwrap();
    let second = agent.end_session().unwrap().expect("an updated note");
    assert_eq!(second.id, first.id);
    assert_eq!(second.title, first.title);
    assert!(second.body.contains("coffee") && second.body.contains("[[coffee]]"), "{}", second.body);
    assert_eq!(session_notes(&agent).len(), 1);
}

#[test]
fn a_short_session_writes_no_note() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "short");
    assert!(agent.end_session().unwrap().is_none(), "an empty session");
    agent.send("what is my cat called?", |_| {}).unwrap();
    assert!(!agent.wants_session_note());
    assert!(agent.end_session().unwrap().is_none(), "a single question");
    assert!(session_notes(&agent).is_empty());
}

#[test]
fn compressing_writes_a_session_note_and_the_session_does_not_recall_its_own() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "compress");
    agent.cfg.num_ctx = 512;
    for i in 0..3 {
        agent.msgs.push(cortex_core::backend::Msg::new("user", format!("question {i} {}", "padding ".repeat(200))));
        agent.msgs.push(cortex_core::backend::Msg::new("assistant", "an answer"));
    }
    agent.send("hello", |_| {}).unwrap(); // compressed automatically first
    let notes = session_notes(&agent);
    assert_eq!(notes.len(), 1, "automatic compression wrote a note");
    assert!(notes[0].body.contains("question 0"), "{}", notes[0].body);

    agent.send("what is my cat called?", |_| {}).unwrap();
    agent.compress().unwrap(); // and /compress
    let notes = session_notes(&agent);
    assert_eq!(notes.len(), 1);
    assert!(notes[0].body.contains("[[cat]]"), "{}", notes[0].body);

    agent.cfg.memory_budget = 0.5; // room for the session note, were it recalled
    agent.send("question padding answer", |_| {}).unwrap();
    assert!(!sent_user_message(&server).contains("[[session-"), "its own summary is already in the conversation");
}

#[test]
fn notes_the_session_remembered_or_looked_up_are_linked_from_its_note() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "tools_link");
    agent.cfg.memory_budget = 0.0; // only the tools bring notes in
    run_turn(&mut agent, "please remember that the dog is called Rex", Approval::AllowOnce).0.unwrap();
    run_turn(&mut agent, "recall coffee", Approval::Deny).0.unwrap();
    let dog = agent.memory.notes().into_iter().find(|n| n.body.contains("Rex")).unwrap();
    let note = agent.end_session().unwrap().expect("a session note");
    assert!(note.body.contains(&format!("[[{}]]", dog.id)), "what it remembered: {}", note.body);
    assert!(note.body.contains("[[coffee]]"), "what it recalled: {}", note.body);
    assert!(!note.body.contains("[[cat]]"), "{}", note.body);
}

#[test]
fn a_stopped_summary_writes_no_note() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "stopped");
    agent.send("hello", |_| {}).unwrap();
    agent.send("a lengthy chat", |_| {}).unwrap();
    let cancel = agent.cancel.clone();
    let stopper = std::thread::spawn(move || { std::thread::sleep(std::time::Duration::from_millis(300)); cancel.cancel(); });
    assert!(agent.end_session().is_err());
    stopper.join().unwrap();
    assert!(session_notes(&agent).is_empty());
    assert!(agent.wants_session_note(), "still to be written");
}

#[test]
fn a_resumed_session_keeps_the_links_it_had() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "resume");
    agent.send("what is my cat called?", |_| {}).unwrap();
    agent.send("hello", |_| {}).unwrap();
    agent.end_session().unwrap().expect("a session note");

    let mut later = self::agent(&server);
    later.memory = cortex_core::memory::Memory::open(agent.memory.dir().parent().unwrap());
    later.resume(cortex_core::agent::load_session(&agent.session_id).unwrap());
    later.cfg.memory_budget = 0.0; // so the cat note can only come from before the resume
    run_turn(&mut later, "recall coffee", Approval::Deny).0.unwrap();
    let note = later.end_session().unwrap().expect("the note, updated");
    assert!(note.body.contains("[[cat]]") && note.body.contains("[[coffee]]"), "{}", note.body);
    assert_eq!(session_notes(&later).len(), 1);
}

#[test]
fn compressing_a_single_exchange_writes_no_note() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "compress_short");
    agent.send("hello", |_| {}).unwrap();
    agent.compress().unwrap();
    assert!(session_notes(&agent).is_empty());
}

// ---------------------------------------------------------------- candidate memories and review

#[test]
fn suggested_memories_are_held_for_review_not_saved() {
    use cortex_core::memory::Proposal;
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "cand_held");
    let mut collected = vec![];
    let mut approvals = 0;
    agent.send("I learned that the cat sleeps on the piano, related to Cat", |e| match e {
        Event::MemoryCandidate(c) => collected.push(c),
        Event::ApprovalRequest(_) => approvals += 1,
        _ => {}
    }).unwrap();
    assert_eq!(collected.len(), 1, "front-ends are told about the candidate");
    assert_eq!((collected[0].body.as_str(), collected[0].related.clone()), ("the cat sleeps on the piano", vec!["Cat".to_string()]));
    assert_eq!(approvals, 0, "nothing is saved yet, so nothing to approve");
    assert!(!agent.memory.notes().iter().any(|n| n.body.contains("piano")), "nothing saved before review");
    assert_eq!(agent.to_review(false), vec![Proposal::New(collected[0].clone())]);

    // kept with the session: resuming it brings the candidate back
    let mut later = self::agent(&server);
    later.memory = cortex_core::memory::Memory::open(agent.memory.dir().parent().unwrap());
    later.resume(cortex_core::agent::load_session(&agent.session_id).unwrap());
    assert_eq!(later.to_review(false), vec![Proposal::New(collected[0].clone())]);
}

#[test]
fn a_candidate_already_in_memory_or_already_suggested_is_not_added_again() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "cand_dupes");
    let (_, trace) = run_turn(&mut agent, "I learned that Drinks black coffee in the morning.", Approval::Deny);
    assert!(trace[1].contains("already in memory") && trace[1].contains("[[coffee]]"), "{}", trace[1]);
    run_turn(&mut agent, "I learned that the vet is closed on Mondays", Approval::Deny).0.unwrap();
    let (_, trace) = run_turn(&mut agent, "I learned that The vet is closed on Mondays", Approval::Deny);
    assert!(trace[1].contains("already suggested"), "{}", trace[1]);
    assert_eq!(agent.to_review(false).len(), 1);
}

#[test]
fn an_approved_candidate_becomes_a_linked_note_that_the_session_note_links() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "cand_approve");
    agent.cfg.memory_budget = 0.0; // so the session note can only link what was approved
    agent.send("I learned that the cat sleeps on the piano, related to Cat", |_| {}).unwrap();
    agent.send("hello", |_| {}).unwrap();
    let items = agent.to_review(false);
    agent.accept(&items[0], None).unwrap();

    let note = agent.memory.notes().into_iter().find(|n| n.body.contains("piano")).expect("saved once approved");
    assert_eq!((note.kind.as_str(), note.source.as_deref()), ("fact", Some(agent.session_id.as_str())));
    assert!(note.body.contains("[[cat]]"), "linked to the note it relates to: {}", note.body);
    assert!(agent.to_review(false).is_empty(), "reviewed");
    let session = agent.end_session().unwrap().expect("a session note");
    assert!(session.body.contains(&format!("[[{}]]", note.id)), "{}", session.body);
}

#[test]
fn an_edited_candidate_is_saved_as_edited_and_a_rejected_one_is_dropped_for_good() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "cand_edit");
    agent.send("I learned that the cat sleeps on the piano", |_| {}).unwrap();
    agent.send("I learned that the vet is closed on Mondays", |_| {}).unwrap();
    let items = agent.to_review(false);
    agent.accept(&items[0], Some("The cat sleeps on the grand piano.")).unwrap();
    agent.reject(&items[1]);

    let bodies: Vec<String> = agent.memory.notes().into_iter().map(|n| n.body).collect();
    assert!(bodies.iter().any(|b| b == "The cat sleeps on the grand piano."), "{bodies:?}");
    assert!(!bodies.iter().any(|b| b.contains("Mondays") || b == "the cat sleeps on the piano"), "{bodies:?}");
    // the review is saved with the session too
    let mut later = self::agent(&server);
    later.memory = cortex_core::memory::Memory::open(agent.memory.dir().parent().unwrap());
    later.resume(cortex_core::agent::load_session(&agent.session_id).unwrap());
    assert!(later.to_review(false).is_empty());
}

#[test]
fn consolidating_reviews_the_candidates_and_the_whole_memory() {
    use cortex_core::memory::{NewNote, Proposal};
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "cand_consolidate");
    agent.memory.add(NewNote { title: "Old".into(), body: "Saw [[Gone]] once.".into(), kind: "fact".into(), source: None }).unwrap();
    agent.send("I learned that the vet is closed on Mondays", |_| {}).unwrap();
    let items = agent.to_review(true);
    assert!(matches!(items[0], Proposal::New(_)), "the session's candidates first: {items:?}");
    assert!(items.iter().any(|p| matches!(p, Proposal::FixLink { target, .. } if target == "Gone")), "{items:?}");
    assert_eq!(agent.to_review(false).len(), 1, "at session end, only the candidates");
}

#[test]
fn a_saved_candidate_with_links_is_still_recognised_when_suggested_again() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "cand_again");
    agent.send("I learned that the cat sleeps on the piano, related to Cat", |_| {}).unwrap();
    let items = agent.to_review(false);
    agent.accept(&items[0], None).unwrap();
    let (_, trace) = run_turn(&mut agent, "I learned that the cat sleeps on the piano, related to Cat", Approval::Deny);
    assert!(trace[1].contains("already in memory"), "{}", trace[1]);
    assert!(agent.to_review(false).is_empty());
}

#[test]
fn undoing_a_turn_drops_the_memories_it_suggested() {
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "cand_undo");
    agent.send("I learned that the vet is closed on Mondays", |_| {}).unwrap();
    agent.send("I learned that the cat sleeps on the piano", |_| {}).unwrap();
    agent.undo();
    let left: Vec<String> = agent.to_review(false).iter().map(|p| p.text()).collect();
    assert_eq!(left, ["the vet is closed on Mondays"]);
}

#[test]
fn reviewed_tidy_ups_are_carried_out_through_the_session() {
    use cortex_core::memory::{NewNote, Proposal};
    let server = MockServer::start();
    let mut agent = agent_with_pets(&server, "cand_fix");
    let old = agent.memory.add(NewNote { title: "Old".into(), body: "Saw [[Gone]] once.".into(), kind: "fact".into(), source: None }).unwrap();
    let old = match old { cortex_core::memory::Added::Created(n) | cortex_core::memory::Added::Exists(n) => n };
    let fix = agent.to_review(true).into_iter().find(|p| matches!(p, Proposal::FixLink { .. })).unwrap();
    agent.accept(&fix, Some("cat")).unwrap();
    assert_eq!(agent.memory.find(&old.id).unwrap().body, "Saw [[cat]] once.");
}

// ---------------------------------------------------------------- semantic recall

/// An agent whose memory holds a cat note and a pet-shop note (see the memory tests).
fn agent_with_pet_notes(server: &MockServer, name: &str) -> Agent {
    use cortex_core::memory::{Memory, NewNote};
    let mut agent = agent(server);
    agent.memory = Memory::open(&scratch_dir(&format!("semantic_{name}")));
    for (title, body) in [("Cat", "Biscuit is the user's pet cat."), ("Pet shop", "The pet shop sells pet food, pet beds and pet toys.")] {
        agent.memory.add(NewNote { title: title.into(), body: body.into(), kind: "fact".into(), source: None }).unwrap();
    }
    agent
}

fn recalled(agent: &mut Agent, text: &str) -> Vec<String> {
    let mut titles = vec![];
    agent.send(text, |e| if let Event::Recalled { notes } = e { titles = notes }).unwrap();
    titles
}

fn embedded(server: &MockServer) -> u64 {
    let v: serde_json::Value = ureq::get(&format!("{}/debug/embedded", server.url())).call().unwrap().into_json().unwrap();
    v["texts"].as_u64().unwrap()
}

#[test]
fn an_installed_embedding_model_is_used_for_recall_with_no_setup() {
    let server = MockServer::start();
    let mut agent = agent_with_pet_notes(&server, "ollama");
    assert_eq!(recalled(&mut agent, "tell me about my feline pet"), ["Pet shop", "Cat"], "no embedding model: keywords alone");
    assert_eq!(embedded(&server), 0);

    server.install("nomic-embed-text");
    let mut agent = agent_with_pet_notes(&server, "ollama2");
    assert_eq!(recalled(&mut agent, "tell me about my feline pet"), ["Cat", "Pet shop"], "found and used without any setting");
    assert_eq!(embedded(&server), 3, "the message and both notes");
    recalled(&mut agent, "tell me about my feline pet");
    assert_eq!(embedded(&server), 4, "the notes' vectors are reused");
}

#[test]
fn embedding_models_on_openai_compatible_servers_are_used_too() {
    let server = MockServer::start();
    server.install("text-embedding-nomic-embed-text-v1.5");
    let mut agent = agent_with_pet_notes(&server, "openai");
    agent.cfg.backend = cortex_core::config::BackendKind::Openai;
    agent.cfg.base_url = format!("{}/v1", server.url());
    agent.backend = cortex_core::backend::Backend::new(&agent.cfg);
    assert_eq!(recalled(&mut agent, "tell me about my feline pet"), ["Cat", "Pet shop"]);
}

#[test]
fn the_recall_tool_puts_the_closest_meaning_first_too() {
    let server = MockServer::start();
    server.install("nomic-embed-text");
    let mut agent = agent_with_pet_notes(&server, "tool");
    agent.cfg.memory_budget = 0.0; // only the tool
    let (_, trace) = run_turn(&mut agent, "recall feline pet", Approval::Deny);
    let (cat, shop) = (trace[1].find("## Cat").unwrap(), trace[1].find("## Pet shop").unwrap());
    assert!(cat < shop, "{}", trace[1]);
}

#[test]
fn a_server_that_was_down_when_first_asked_is_asked_again() {
    use cortex_core::memory::Embedder;
    let server = MockServer::start();
    let port = server.port;
    let cfg = Config { base_url: server.url(), model: "mock:7b".into(), ..Config::default() };
    drop(server);
    let backend = cortex_core::backend::Backend::new(&cfg);
    assert_eq!(backend.model(), None, "the server is down: keywords alone for now");
    let server = MockServer::start_on(port);
    server.install("nomic-embed-text");
    assert_eq!(backend.model().as_deref(), Some("nomic-embed-text"), "not remembered as \"no model\"");
}

#[test]
fn a_model_without_tool_support_falls_back_to_plain_chat_once() {
    let server = MockServer::start();
    let mut agent = agent(&server);
    agent.model = "notools".into(); // the fake server refuses tools for this model, as Ollama does for some
    let mut fallbacks = 0;
    let answer = agent.send("what is 2+2?", |e| if let Event::PlainChatFallback(_) = e { fallbacks += 1 }).unwrap();
    assert_eq!(fallbacks, 1, "the front-end is told once");
    assert_eq!(answer.trim(), "Plain chat answer without tools.", "and the model still answers");
    let mut again = 0;
    agent.send("and 3+3?", |e| if let Event::PlainChatFallback(_) = e { again += 1 }).unwrap();
    assert_eq!(again, 0, "later turns go straight to plain chat");
    let req: serde_json::Value = ureq::get(&format!("{}/debug/last-request", server.url())).call().unwrap().into_json().unwrap();
    assert!(req.get("tools").is_none_or(|t| t.as_array().is_some_and(|a| a.is_empty())), "no tools are offered any more: {req}");
}
