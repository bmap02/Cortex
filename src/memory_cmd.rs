//! `cortex memory list | show | search | links | delete | clear | path`: long-term memory from the
//! terminal.

use crate::ui;
use cortex_core::config;
use cortex_core::memory::{Memory, Note};

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

fn search(mem: &Memory, query: &str) -> i32 {
    let hits = mem.search(query, 10);
    if hits.is_empty() { println!("{}", ui::muted(&format!("nothing in memory matches '{query}'"))); }
    for h in &hits {
        let snippet = ui::truncate(&cortex_core::memory::one_line(&h.note.body), 70);
        println!("  {}  {}", ui::bold(&format!("{:<28}", h.note.id)), ui::faint(&snippet));
    }
    0
}
