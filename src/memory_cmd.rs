//! `cortex memory list | show | search | links | path`: long-term memory from the terminal.

use crate::ui;
use cortex_core::config;
use cortex_core::memory::{Memory, Note};

pub fn run(action: &str, args: &[String]) -> i32 {
    let mem = Memory::open(&config::home());
    let arg = args.join(" ");
    match (action, arg.trim()) {
        ("list", _) => { list(&mem); 0 }
        ("show" | "links" | "search", "") => {
            eprintln!("usage: cortex memory {action} <{}>", if action == "search" { "words" } else { "note id or title" });
            2
        }
        ("show", name) => with_note(&mem, name, |n| show(&mem, n)),
        ("links", name) => with_note(&mem, name, |n| links(&mem, n)),
        ("search", query) => search(&mem, query),
        ("path", _) => { println!("{}", mem.dir().display()); 0 }
        _ => { eprintln!("unknown memory command '{action}'. Try list, show, search, links or path"); 2 }
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
    if let Some(s) = &n.source { meta.push(format!("from session {s}")); }
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
            None => println!("  → {}  {}", link.target, ui::faint("(no such note yet)")),
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
