//! Drives a core agent session through its public interface against the scripted fake model
//! server (tests/mock_server.py): start a turn, collect the events, answer approvals, cancel.

use cortex_core::agent::{Agent, Approval, Event, StopReason};
use cortex_core::config::Config;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Once;

/// A fake model server on a free port, stopped when dropped.
struct MockServer { child: Child, port: u16 }

impl MockServer {
    fn start() -> MockServer {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/mock_server.py");
        // python3 first; on Windows it can be a Store placeholder that prints nothing, so fall through
        for py in ["python3", "python"] {
            let Ok(mut child) = Command::new(py).args(["-u", script, "0"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { continue };
            let mut line = String::new();
            let _ = BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line);
            match line.trim().strip_prefix("PORT ").and_then(|p| p.parse().ok()) {
                Some(port) => return MockServer { child, port },
                None => { let _ = child.kill(); let _ = child.wait(); }
            }
        }
        panic!("python is needed to run the fake model server (tests/mock_server.py)")
    }
}

impl Drop for MockServer {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

/// Sessions save under the Cortex home folder: keep them in a temporary one.
fn temp_home() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let home = std::env::temp_dir().join(format!("cortex_session_tests{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: set once, before any test reads the environment (every test calls this first)
        unsafe { std::env::set_var("CORTEX_HOME", home) };
    });
}

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
