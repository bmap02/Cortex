//! `cortex memory list | show | search | links | delete | clear | path`: long-term memory from the
//! terminal.

use crate::ui;
use cortex_core::agent::Agent;
use cortex_core::config;
use cortex_core::memory::{Memory, Note, Proposal};
use std::io::IsTerminal;

/// `yes` skips the confirmation before deleting.
pub fn run(action: &str, args: &[String], yes: bool) -> i32 {
    let mem = Memory::open(&config::home());
    let arg = args.join(" ");
    match (action, arg.trim()) {
        ("list", _) => { list(&mem); 0 }
        ("show" | "links" | "search" | "delete", "") => {
            eprintln!("usage: cortex memory {action} <{}>", if action == "search" { "words" } else { "note id or title" });
            2
        }
        ("show", name) => with_note(&mem, name, |n| show(&mem, n)),
        ("links", name) => with_note(&mem, name, |n| links(&mem, n)),
        ("search", query) => search(&mem, query),
        ("delete" | "rm", name) => delete(&mem, name, yes),
        ("clear", _) => clear(&mem, yes),
        ("path", _) => { println!("{}", mem.dir().display()); 0 }
        _ => { eprintln!("unknown memory command '{action}'. Try list, show, search, links, delete, clear or path"); 2 }
    }
}

/// Ask before deleting, unless --yes. Without a terminal to ask at, the answer is no.
fn confirm(question: &str, yes: bool) -> bool {
    yes || crate::ask(&format!("{question} [y/N]"), "").eq_ignore_ascii_case("y")
}

fn delete(mem: &Memory, name: &str, yes: bool) -> i32 {
    let Some(note) = mem.find(name) else {
        eprintln!("{}", ui::error(&format!("no note called '{name}' (see `cortex memory list`)")));
        return 1;
    };
    if !confirm(&format!("delete the note '{}'?", note.title), yes) {
        println!("{}", ui::muted("kept it"));
        return 1;
    }
    match mem.delete(&note.id) {
        Ok(d) => {
            println!("{}", ui::muted(&format!("deleted {}", d.deleted.id)));
            if !d.linked_from.is_empty() {
                let names: Vec<&str> = d.linked_from.iter().map(|n| n.id.as_str()).collect();
                println!("{}", ui::faint(&format!("  these notes still link to it: {}", names.join(", "))));
            }
            0
        }
        Err(e) => { eprintln!("{}", ui::error(&e)); 1 }
    }
}

/// `cortex memory clear` and `/forget`: delete every note, after asking.
pub fn clear(mem: &Memory, yes: bool) -> i32 {
    let n = mem.notes().len();
    if n == 0 { println!("{}", ui::muted("memory is already empty")); return 0; }
    if !confirm(&format!("delete all {}? This can't be undone.", ui::plural(n, "memory note")), yes) {
        println!("{}", ui::muted("kept them"));
        return 1;
    }
    match mem.clear() {
        Ok(d) => { println!("{}", ui::muted(&format!("deleted {}", ui::plural(d, "note")))); 0 }
        Err(e) => { eprintln!("{}", ui::error(&e)); 1 }
    }
}

fn with_note(mem: &Memory, name: &str, f: impl FnOnce(&Note)) -> i32 {
    match mem.find(name) {
        Some(n) => { f(&n); 0 }
        None => { eprintln!("{}", ui::error(&format!("no note called '{name}' (see `cortex memory list`)"))); 1 }
    }
}

/// One line per note: id, title, kind and date.
pub fn list(mem: &Memory) {
    let notes = mem.notes();
    if notes.is_empty() {
        println!("{}", ui::muted("no memories yet. Ask Cortex to remember something."));
        return;
    }
    for n in &notes {
        let date = n.updated.get(..10).unwrap_or("");
        println!("  {}  {}  {}", ui::bold(&format!("{:<28}", n.id)), ui::truncate(&n.title, 50), ui::faint(&format!("{} {date}", n.kind)));
    }
    println!("{}", ui::faint(&format!("  {} in {}", ui::plural(notes.len(), "note"), mem.dir().display())));
}

fn show(mem: &Memory, n: &Note) {
    println!("{}", ui::bold(&n.title));
    let mut meta = vec![n.kind.clone(), n.id.clone()];
    if !n.created.is_empty() { meta.push(format!("created {}", n.created)); }
    if let Some(s) = &n.source {
        // the conversation may have been cleared since; the note outlives it
        let kept = config::sessions_dir().join(format!("{s}.json")).exists();
        meta.push(format!("from session {s}{}", if kept { "" } else { " (deleted)" }));
    }
    println!("{}", ui::faint(&meta.join(" · ")));
    println!("\n{}\n", n.body);
    links(mem, n);
    println!("{}", ui::faint(&n.path.display().to_string()));
}

/// The note's links, and the notes that link to it.
fn links(mem: &Memory, n: &Note) {
    let l = mem.links(n);
    println!("{}", ui::bold("links to"));
    if l.outgoing.is_empty() { println!("  {}", ui::faint("nothing")); }
    for link in &l.outgoing {
        match &link.note {
            Some(t) => println!("  → {}  {}", t.id, ui::faint(&t.title)),
            None => println!("  → {}  {}", link.target, ui::faint(&format!("(no note called '{}')", link.target))),
        }
    }
    println!("{}", ui::bold("linked from"));
    if l.incoming.is_empty() { println!("  {}", ui::faint("nothing")); }
    for t in &l.incoming { println!("  ← {}  {}", t.id, ui::faint(&t.title)); }
}

/// What the user said about one item in a review.
pub enum Decision<'a> {
    /// Do it, with the text the user typed if they edited it.
    Accept(Option<&'a str>),
    Reject,
}

/// Review the memories the agent suggested (and, with `consolidate`, ways to tidy memory) at the
/// terminal. Without one (piped input) they're left for a later review.
pub fn review_in_terminal(agent: &mut Agent, consolidate: bool) {
    let items = agent.to_review(consolidate);
    if items.is_empty() {
        if consolidate { println!("{}", ui::muted("nothing to review: no suggested memories, and memory looks tidy")); }
        return;
    }
    if !std::io::stdin().is_terminal() {
        if let Some(h) = pending_hint(agent) { println!("{}", ui::faint(&h)); }
        return;
    }
    let Ok(mut rl) = rustyline::DefaultEditor::new() else { return };
    let mut ask = |prompt: &str, initial: &str| rl.readline_with_initial(prompt, (initial, "")).ok();
    review(&items, &mut ask, &mut |p, d| match d {
        Decision::Accept(edit) => agent.accept(p, edit).map(|n| match p {
            Proposal::New(_) => format!("in memory as {}", n.id), // new, or a note it matched
            Proposal::Merge { .. } => format!("merged into {}", n.id),
            Proposal::Link { .. } | Proposal::FixLink { .. } => format!("updated {}", n.id),
        }),
        Decision::Reject => { agent.reject(p); Ok(String::new()) }
    });
    if let Some(h) = pending_hint(agent) { println!("{}", ui::faint(&h)); } // stopped part-way
}

/// Where to review the memories this session suggested, if any are waiting.
pub fn pending_hint(agent: &Agent) -> Option<String> {
    let n = agent.to_review(false).len();
    (n > 0).then(|| format!("{} to review later: cortex -r {}, then /consolidate", ui::plural(n, "suggested memory"), agent.session_id))
}

/// Go through `items` one by one: y does it, e edits it first, n turns it down, and Y or N answer
/// this one and all the rest. `ask(prompt, initial text)` reads a line; None (Ctrl+C, Ctrl+D, end
/// of input) stops the review and leaves the rest for later. `decide` carries out each answer and
/// says what it did.
pub fn review(items: &[Proposal], ask: &mut dyn FnMut(&str, &str) -> Option<String>,
              decide: &mut dyn FnMut(&Proposal, Decision) -> Result<String, String>) {
    println!("{}", ui::bold(&format!("Review {}:", ui::plural(items.len(), "suggested memory change"))));
    for (i, p) in items.iter().enumerate() { println!("  {}. {}", i + 1, describe(p)); }
    let mut rest: Option<bool> = None; // Y or N: the answer for every item left
    for (i, p) in items.iter().enumerate() {
        let answer = match rest {
            Some(all) => if all { "y".to_string() } else { "n".to_string() },
            None => loop {
                let prompt = format!("  [{}/{}] {}  y/e/n (Y/N: all the rest) ", i + 1, items.len(), ui::truncate(&describe(p), 60));
                let Some(a) = ask(&prompt, "") else { return };
                match a.trim() {
                    "Y" => { rest = Some(true); break "y".into() }
                    "N" => { rest = Some(false); break "n".into() }
                    a if ["y", "yes", "e", "edit", "n", "no"].contains(&a.to_lowercase().as_str()) => break a.to_lowercase(),
                    _ => println!("{}", ui::faint("  type y (yes), e (edit) or n (no)")),
                }
            },
        };
        let result = match answer.as_str() {
            "e" | "edit" => {
                let Some(text) = ask("  edit: ", &p.text()) else { return };
                // nothing left to save; for a dead link, empty means "make it plain text"
                if text.trim().is_empty() && !matches!(p, Proposal::FixLink { .. }) { decide(p, Decision::Reject) }
                else { decide(p, Decision::Accept(Some(&text))) }
            }
            "y" | "yes" => decide(p, Decision::Accept(None)),
            _ => decide(p, Decision::Reject),
        };
        match result {
            Ok(done) if !done.is_empty() => println!("{}", ui::faint(&format!("    {done}"))),
            Ok(_) => {}
            Err(e) => println!("{}", ui::error(&format!("    {e}"))),
        }
    }
}

/// One line saying what a proposal would do.
fn describe(p: &Proposal) -> String {
    let one = |s: &str| ui::clean(&cortex_core::memory::one_line(s)); // text from the model or a file
    match p {
        Proposal::New(c) if c.related.is_empty() => format!("remember: {}", one(&c.body)),
        Proposal::New(c) => format!("remember: {}  (related: {})", one(&c.body), one(&c.related.join(", "))),
        Proposal::Merge { keep, remove, body } => format!("merge {remove} into {keep}: {}", one(body)),
        Proposal::Link { from, to } => format!("link {from} → {to}"),
        Proposal::FixLink { note, target, to: Some(to) } => format!("in {note}, point the dead link [[{}]] at {to}", one(target)),
        Proposal::FixLink { note, target, to: None } => format!("in {note}, make the dead link [[{}]] plain text", one(target)),
    }
}

fn search(mem: &Memory, query: &str) -> i32 {
    let hits = mem.search(query, 10);
    if hits.is_empty() { println!("{}", ui::muted(&format!("nothing in memory matches '{query}'"))); }
    for h in &hits {
        let snippet = ui::truncate(&cortex_core::memory::one_line(&h.note.body), 70);
        println!("  {}  {}", ui::bold(&format!("{:<28}", h.note.id)), ui::faint(&snippet));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{review, Decision};
    use cortex_core::memory::{Candidate, Proposal};

    fn items(n: usize) -> Vec<Proposal> {
        (1..=n).map(|i| Proposal::New(Candidate { body: format!("fact {i}"), kind: "fact".into(), related: vec![] })).collect()
    }

    /// Run a review with scripted answers; returns what was decided for each item, in order.
    fn run(items: &[Proposal], answers: &[&str]) -> Vec<String> {
        let mut answers = answers.iter();
        let mut log = vec![];
        let mut ask = |prompt: &str, initial: &str| {
            if prompt.contains("edit") { assert_eq!(initial, "fact 2", "the edit starts from the current text"); }
            answers.next().map(|a| a.to_string())
        };
        review(items, &mut ask, &mut |p, d| {
            log.push(match d { Decision::Accept(None) => format!("save {}", p.text()), Decision::Accept(Some(t)) => format!("save {t}"), Decision::Reject => format!("drop {}", p.text()) });
            Ok(String::new())
        });
        log
    }

    #[test]
    fn each_item_is_saved_edited_or_dropped_and_the_rest_wait_when_input_ends() {
        let log = run(&items(4), &["y", "e", "fact two, edited", "what", "n"]);
        assert_eq!(log, ["save fact 1", "save fact two, edited", "drop fact 3"], "an unclear answer is asked again; fact 4 waits");
    }

    #[test]
    fn an_edit_left_empty_drops_the_item_instead_of_saving_nothing() {
        assert_eq!(run(&items(2), &["y", "e", "  "]), ["save fact 1", "drop fact 2"]);
    }

    #[test]
    fn capital_y_or_n_answers_this_item_and_all_the_rest() {
        assert_eq!(run(&items(3), &["n", "Y"]), ["drop fact 1", "save fact 2", "save fact 3"]);
        assert_eq!(run(&items(3), &["N"]), ["drop fact 1", "drop fact 2", "drop fact 3"]);
    }
}
