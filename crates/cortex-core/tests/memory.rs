//! Memory as a folder of linked Markdown notes: the files are the source of truth, and the search
//! index is a cache rebuilt from them.

use cortex_core::memory::{Added, Memory, NewNote};
use std::path::PathBuf;

/// A fresh, empty Cortex home folder for one test.
fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cortex_mem_{name}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn note(title: &str, body: &str) -> NewNote {
    NewNote { title: title.into(), body: body.into(), kind: "fact".into(), source: Some("20261112-150000".into()) }
}

fn added(a: Added) -> cortex_core::memory::Note { match a { Added::Created(n) | Added::Exists(n) => n } }

#[test]
fn a_note_is_a_plain_markdown_file_with_frontmatter() {
    let h = home("file");
    let mem = Memory::open(&h);
    let n = added(mem.add(note("Prefers dark mode", "The user prefers dark mode in every editor.")).unwrap());

    let path = h.join("memory").join(format!("{}.md", n.id));
    assert_eq!(n.path, path);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("---\n"), "{text}");
    for field in [format!("id: {}", n.id), "title: Prefers dark mode".into(), "kind: fact".into(), "source: 20261112-150000".into()] {
        assert!(text.contains(&field), "missing {field:?} in\n{text}");
    }
    assert!(text.contains("created: 20") && text.contains("updated: 20"));
    assert!(text.trim_end().ends_with("The user prefers dark mode in every editor."));
    assert_eq!(n.id, "prefers-dark-mode");
}

#[test]
fn the_same_fact_twice_is_kept_once_and_titles_get_unique_ids() {
    let mem = Memory::open(&home("dupes"));
    assert!(matches!(mem.add(note("Uses Rust", "Works mostly in Rust.")).unwrap(), Added::Created(_)));
    assert!(matches!(mem.add(note("Rust user", "works mostly in rust.")).unwrap(), Added::Exists(_)), "same body, any case");
    let second = added(mem.add(note("Uses Rust", "Also writes some Go.")).unwrap());
    assert_eq!(second.id, "uses-rust-2");
    assert_eq!(mem.notes().len(), 2);
}

#[test]
fn notes_are_found_by_id_or_title_and_links_go_both_ways() {
    let mem = Memory::open(&home("links"));
    added(mem.add(note("Cortex project", "A local AI agent in Rust. See [[Rust toolchain]] and [[nowhere]].")).unwrap());
    added(mem.add(note("Rust toolchain", "Uses stable Rust with clippy.")).unwrap());
    added(mem.add(note("Release steps", "Tag after CI passes on [[cortex-project]].")).unwrap());

    assert_eq!(mem.find("cortex-project").unwrap().title, "Cortex project");
    assert_eq!(mem.find("rust TOOLCHAIN").unwrap().id, "rust-toolchain", "titles match in any case");
    let links = mem.links(&mem.find("cortex-project").unwrap());
    let out: Vec<(String, Option<String>)> = links.outgoing.iter().map(|l| (l.target.clone(), l.note.as_ref().map(|n| n.id.clone()))).collect();
    assert_eq!(out, [("Rust toolchain".into(), Some("rust-toolchain".into())), ("nowhere".into(), None)]);
    assert_eq!(links.incoming.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["release-steps"]);
}

#[test]
fn search_ranks_the_most_relevant_note_first() {
    let mem = Memory::open(&home("search"));
    added(mem.add(note("Editor theme", "Prefers a dark theme in the code editor, with a large font.")).unwrap());
    added(mem.add(note("Coffee", "Drinks black coffee in the morning.")).unwrap());
    added(mem.add(note("Cortex project", "Building Cortex, a local AI agent written in Rust.")).unwrap());

    let hits = mem.search("which editor theme do I like", 5);
    assert_eq!(hits[0].note.id, "editor-theme");
    assert!(hits.iter().all(|h| h.note.id != "coffee"), "unrelated notes don't match: {:?}", hits.iter().map(|h| &h.note.id).collect::<Vec<_>>());
    assert_eq!(mem.search("rust agent", 5)[0].note.id, "cortex-project");
    assert!(mem.search("zebra", 5).is_empty());
}

#[test]
fn the_index_is_rebuilt_when_missing_or_corrupt_and_picks_up_hand_edits() {
    let h = home("index");
    let mem = Memory::open(&h);
    added(mem.add(note("Editor theme", "Prefers a dark theme.")).unwrap());
    assert_eq!(mem.search("dark theme", 5).len(), 1);
    let index = h.join("memory").join(".index.json");
    assert!(index.exists(), "searching writes the index");

    std::fs::remove_file(&index).unwrap();
    assert_eq!(mem.search("dark theme", 5).len(), 1, "a deleted index is rebuilt");
    std::fs::write(&index, "{ not json").unwrap();
    assert_eq!(mem.search("dark theme", 5).len(), 1, "a corrupt index is rebuilt");

    // edited by hand in a text editor: the new words are found, the old ones aren't
    let path = h.join("memory").join("editor-theme.md");
    let text = std::fs::read_to_string(&path).unwrap().replace("Prefers a dark theme.", "Switched to a solarized light palette.");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, text).unwrap();
    assert_eq!(mem.search("solarized palette", 5).len(), 1);
    assert!(mem.search("dark", 5).is_empty());
    // a note added by hand is found too
    std::fs::write(h.join("memory").join("by-hand.md"), "---\ntitle: Written by hand\n---\nLikes hiking in the mountains.\n").unwrap();
    assert_eq!(mem.search("hiking", 5)[0].note.title, "Written by hand");
}

#[test]
fn an_old_memory_file_is_split_into_notes_once_and_kept_as_a_backup() {
    let h = home("migrate");
    let old = "# Memory\n- Prefers dark mode\n- Works mostly in Rust\n\n- Lives in Berlin\n";
    std::fs::write(h.join("MEMORY.md"), old).unwrap();

    let mem = Memory::open(&h);
    let mut bodies: Vec<String> = mem.notes().into_iter().map(|n| n.body).collect();
    bodies.sort();
    assert_eq!(bodies, ["Lives in Berlin", "Prefers dark mode", "Works mostly in Rust"]);
    assert!(!h.join("MEMORY.md").exists());
    assert_eq!(std::fs::read_to_string(h.join("MEMORY.md.bak")).unwrap(), old, "the original is kept as it was");
    assert_eq!(Memory::open(&h).notes().len(), 3, "opening again doesn't migrate twice");
}

#[test]
fn hand_written_notes_are_read_however_they_are_laid_out() {
    let h = home("layout");
    let dir = h.join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bom.md"), "\u{feff}---\ntitle: Saved by Notepad\nkind: project\n---\nWith a byte order mark.\n").unwrap();
    std::fs::write(dir.join("empty-front.md"), "---\n---\nNo fields at all.\n").unwrap();
    std::fs::write(dir.join("rule.md"), "---\ntitle: Has a rule\n----\nnot the end\n---\nThe body.\n").unwrap();
    std::fs::write(dir.join("plain.md"), "Just text, no frontmatter.\r\nSecond line.\r\n").unwrap();
    let mem = Memory::open(&h);

    let bom = mem.find("bom").unwrap();
    assert_eq!((bom.title.as_str(), bom.kind.as_str(), bom.body.as_str()), ("Saved by Notepad", "project", "With a byte order mark."));
    assert_eq!(mem.find("empty-front").unwrap().body, "No fields at all.");
    let rule = mem.find("rule").unwrap();
    assert_eq!(rule.title, "Has a rule");
    assert_eq!(rule.body, "The body.", "only a line of exactly --- closes the frontmatter");
    assert_eq!(mem.find("plain").unwrap().body, "Just text, no frontmatter.\nSecond line.");
    assert!(!mem.find("plain").unwrap().updated.is_empty(), "the file's time stands in for a missing date");
}

#[test]
fn titles_in_any_language_make_readable_ids() {
    let mem = Memory::open(&home("unicode"));
    assert_eq!(added(mem.add(note("Café in München", "Likes the café.")).unwrap()).id, "café-in-münchen");
    assert_eq!(added(mem.add(note("日本語のメモ", "A note in Japanese.")).unwrap()).id, "日本語のメモ");
}

#[test]
fn two_cortex_windows_saving_at_once_keep_both_notes() {
    let h = home("race");
    let saves: Vec<_> = (0..8).map(|i| {
        let h = h.clone();
        std::thread::spawn(move || added(Memory::open(&h).add(note("Same title", &format!("Fact number {i}."))).unwrap()).id)
    }).collect();
    let mut ids: Vec<String> = saves.into_iter().map(|t| t.join().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 8, "every note got its own file: {ids:?}");
    assert_eq!(Memory::open(&h).notes().len(), 8);
}

#[test]
fn migrating_from_two_windows_at_once_converts_the_file_once() {
    let h = home("race_migrate");
    std::fs::write(h.join("MEMORY.md"), (1..=20).map(|i| format!("- fact {i}\n")).collect::<String>()).unwrap();
    let opens: Vec<_> = (0..4).map(|_| { let h = h.clone(); std::thread::spawn(move || { Memory::open(&h); }) }).collect();
    for t in opens { t.join().unwrap(); }
    assert_eq!(Memory::open(&h).notes().len(), 20, "no fact twice, none lost");
    assert!(h.join("MEMORY.md.bak").exists());
}

#[test]
fn deleting_a_note_leaves_links_to_it_dead_but_harmless() {
    let h = home("delete");
    let mem = Memory::open(&h);
    added(mem.add(note("Cat", "The cat is called Biscuit; see [[Vet]].")).unwrap());
    added(mem.add(note("Vet", "The vet is Dr Lee.")).unwrap());

    let gone = mem.delete("Vet").unwrap();
    assert_eq!(gone.deleted.id, "vet");
    assert_eq!(gone.linked_from.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["cat"], "says which notes now have a dead link");
    assert!(!h.join("memory").join("vet.md").exists());
    assert!(mem.delete("vet").is_err(), "already gone");

    // the dead link is shown as unresolved, and nothing else minds
    let links = mem.links(&mem.find("cat").unwrap());
    assert!(links.outgoing[0].note.is_none());
    assert_eq!(mem.search("dr lee", 5).len(), 0);
    assert_eq!(mem.search("biscuit", 5)[0].note.id, "cat");
}

#[test]
fn clearing_memory_deletes_the_notes_and_nothing_else() {
    let h = home("clear");
    let mem = Memory::open(&h);
    added(mem.add(note("One", "First fact.")).unwrap());
    added(mem.add(note("Two", "Second fact.")).unwrap());
    mem.search("fact", 5); // writes the index
    std::fs::write(h.join("memory").join("keep.txt"), "not a note").unwrap();

    assert_eq!(mem.clear().unwrap(), 2);
    assert!(mem.notes().is_empty());
    assert!(mem.search("fact", 5).is_empty());
    assert!(h.join("memory").join("keep.txt").exists(), "only notes (and the index) are removed");
}

#[test]
fn apostrophes_dont_split_words_in_ids() {
    let mem = Memory::open(&home("apostrophe"));
    assert_eq!(added(mem.add(note("User's name is Ben", "The user's name is Ben.")).unwrap()).id, "users-name-is-ben");
    assert_eq!(added(mem.add(note("Don’t use tabs", "Prefers spaces.")).unwrap()).id, "dont-use-tabs", "curly apostrophes too");
}

/// A small memory: the cat note matches "cat", links to the vet (which doesn't), and the
/// coffee note is unrelated.
fn pets(name: &str) -> Memory {
    let mem = Memory::open(&home(name));
    added(mem.add(note("Cat", "The cat is called Biscuit. Her vet is [[Vet]]; see also [[Gone]].")).unwrap());
    added(mem.add(note("Vet", "Dr Lee at the corner clinic.")).unwrap());
    added(mem.add(note("Coffee", "Drinks black coffee in the morning.")).unwrap());
    added(mem.add(note("Food", "Biscuit eats salmon; feeding notes for [[Cat]].")).unwrap());
    mem
}

#[test]
fn recall_brings_matching_notes_and_their_neighbours() {
    let mem = pets("recall");
    let ids: Vec<String> = mem.recall("what is my cat called?", 8, 1000).into_iter().map(|n| n.id).collect();
    assert_eq!(ids[0], "cat", "the best match first: {ids:?}");
    assert!(ids.contains(&"vet".to_string()), "one link away (the cat links to it): {ids:?}");
    assert!(ids.contains(&"food".to_string()), "one link away (it links to the cat): {ids:?}");
    assert!(!ids.contains(&"coffee".to_string()), "unrelated: {ids:?}");
    // [[Gone]] points nowhere: skipped, nothing breaks
    assert!(mem.recall("zebra", 8, 1000).is_empty());
}

#[test]
fn recall_never_goes_over_its_budget() {
    let mem = pets("budget");
    let tokens = |notes: &[cortex_core::memory::Note]| notes.iter().map(cortex_core::memory::recall_tokens).sum::<usize>();
    let all = mem.recall("cat biscuit", 8, 1000);
    let full = tokens(&all);
    assert!(all.len() >= 3);
    for budget in [0, 5, full / 2, full - 1, full] {
        let some = mem.recall("cat biscuit", 8, budget);
        assert!(tokens(&some) <= budget, "budget {budget}: used {}", tokens(&some));
    }
    assert_eq!(mem.recall("cat biscuit", 8, full).len(), all.len());
    assert!(mem.recall("cat biscuit", 8, 0).is_empty());
}

#[test]
fn put_writes_a_note_at_its_id_and_rewrites_it_keeping_when_it_was_created() {
    let h = home("put");
    let mem = Memory::open(&h);
    let session = |body: &str| NewNote { title: "Session: cats".into(), body: body.into(), kind: "session".into(), source: Some("20261113-090000".into()) };
    let first = mem.put("session-20261113-090000", session("First summary.")).unwrap();
    assert_eq!((first.id.as_str(), first.kind.as_str(), first.body.as_str()), ("session-20261113-090000", "session", "First summary."));
    // pretend it was written long ago
    let text = std::fs::read_to_string(&first.path).unwrap().replace(&first.created, "2020-01-01T00:00:00Z");
    std::fs::write(&first.path, text).unwrap();

    let second = mem.put("session-20261113-090000", session("Second summary.")).unwrap();
    assert_eq!(second.body, "Second summary.");
    assert_eq!(second.created, "2020-01-01T00:00:00Z", "still created when it was first written");
    assert!(second.updated > second.created);
    assert_eq!(mem.notes().len(), 1, "rewritten, not added again");
    let files: Vec<String> = std::fs::read_dir(mem.dir()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(files.iter().all(|f| f == "session-20261113-090000.md" || f == ".index.json"), "no temporary files left: {files:?}");
    assert!(mem.put("session-x", session("  ")).is_err(), "a note needs some text");
}
