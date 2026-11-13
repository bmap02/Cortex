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
    let ids: Vec<String> = mem.recall("what is my cat called?", 8, 1000, None).into_iter().map(|n| n.id).collect();
    assert_eq!(ids[0], "cat", "the best match first: {ids:?}");
    assert!(ids.contains(&"vet".to_string()), "one link away (the cat links to it): {ids:?}");
    assert!(ids.contains(&"food".to_string()), "one link away (it links to the cat): {ids:?}");
    assert!(!ids.contains(&"coffee".to_string()), "unrelated: {ids:?}");
    // [[Gone]] points nowhere: skipped, nothing breaks
    assert!(mem.recall("zebra", 8, 1000, None).is_empty());
}

#[test]
fn recall_never_goes_over_its_budget() {
    let mem = pets("budget");
    let tokens = |notes: &[cortex_core::memory::Note]| notes.iter().map(cortex_core::memory::recall_tokens).sum::<usize>();
    let all = mem.recall("cat biscuit", 8, 1000, None);
    let full = tokens(&all);
    assert!(all.len() >= 3);
    for budget in [0, 5, full / 2, full - 1, full] {
        let some = mem.recall("cat biscuit", 8, budget, None);
        assert!(tokens(&some) <= budget, "budget {budget}: used {}", tokens(&some));
    }
    assert_eq!(mem.recall("cat biscuit", 8, full, None).len(), all.len());
    assert!(mem.recall("cat biscuit", 8, 0, None).is_empty());
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

// ---------------------------------------------------------------- consolidation

fn add(mem: &Memory, title: &str, body: &str) -> cortex_core::memory::Note { added(mem.add(note(title, body)).unwrap()) }

#[test]
fn consolidation_proposes_merging_near_duplicates_and_changes_nothing_until_applied() {
    use cortex_core::memory::Proposal;
    let mem = Memory::open(&home("merge"));
    add(&mem, "Dark mode", "The user prefers dark mode in every editor.");
    add(&mem, "Dark theme", "The user prefers dark mode in every editor and terminal.");
    add(&mem, "Rust", "Works mostly in Rust.");
    let project = add(&mem, "Project", "Cortex follows [[Dark mode]].");

    let merge = mem.consolidation().into_iter().find(|p| matches!(p, Proposal::Merge { .. })).expect("a merge");
    let Proposal::Merge { keep, remove, body } = &merge else { unreachable!() };
    assert_eq!((keep.as_str(), remove.as_str()), ("dark-theme", "dark-mode"), "keeps the fuller note");
    assert!(body.contains("and terminal") && body.contains("in every editor."), "{body}");
    assert_eq!(mem.notes().len(), 4, "only a proposal so far");

    let kept = mem.apply(&merge, None).unwrap();
    assert_eq!(&kept.body, body, "the kept note, as merged");
    assert!(mem.find("dark-mode").is_none());
    let project = mem.find(&project.id).unwrap();
    assert!(project.body.contains("[[dark-theme]]"), "links to the removed note now point at the kept one: {}", project.body);
    assert!(mem.links(&project).outgoing.iter().all(|l| l.note.is_some()));
    assert!(!mem.consolidation().iter().any(|p| matches!(p, Proposal::Merge { .. })), "nothing left to merge");
}

#[test]
fn consolidation_proposes_links_between_related_notes() {
    use cortex_core::memory::Proposal;
    let mem = Memory::open(&home("consolidate_links"));
    add(&mem, "Cat", "The cat Biscuit sleeps on the piano all day.");
    add(&mem, "Piano", "The piano in the living room is a Yamaha the cat sleeps on.");
    add(&mem, "Coffee", "Black coffee every morning.");

    let links: Vec<Proposal> = mem.consolidation().into_iter().filter(|p| matches!(p, Proposal::Link { .. })).collect();
    assert_eq!(links.len(), 1, "{links:?}");
    let Proposal::Link { from, to } = &links[0] else { unreachable!() };
    let mut pair = [from.as_str(), to.as_str()];
    pair.sort();
    assert_eq!(pair, ["cat", "piano"]);

    mem.apply(&links[0], None).unwrap();
    let from_note = mem.find(from).unwrap();
    assert!(mem.links(&from_note).outgoing.iter().any(|l| l.note.as_ref().is_some_and(|n| &n.id == to)), "{}", from_note.body);
    assert!(!mem.consolidation().iter().any(|p| matches!(p, Proposal::Link { .. })), "already linked now");
}

#[test]
fn consolidation_offers_to_fix_or_remove_dead_links() {
    use cortex_core::memory::Proposal;
    let mem = Memory::open(&home("dead"));
    add(&mem, "Vet", "Dr Lee at the corner clinic.");
    let cat = add(&mem, "Cat", "Biscuit sees the [[Old vet]] about [[Zzyzx]].");

    let fixes: Vec<Proposal> = mem.consolidation().into_iter().filter(|p| matches!(p, Proposal::FixLink { .. })).collect();
    assert_eq!(fixes, vec![
        Proposal::FixLink { note: "cat".into(), target: "Old vet".into(), to: Some("vet".into()) },
        Proposal::FixLink { note: "cat".into(), target: "Zzyzx".into(), to: None },
    ]);
    for f in &fixes { mem.apply(f, None).unwrap(); }
    let body = mem.find(&cat.id).unwrap().body;
    assert_eq!(body, "Biscuit sees the [[vet]] about Zzyzx.", "a link with no match becomes plain text");
    // an edit picks another target, or (empty) removes the link
    assert_eq!(fixes[0].with_text(""), Proposal::FixLink { note: "cat".into(), target: "Old vet".into(), to: None });
    assert_eq!(fixes[1].with_text("vet"), Proposal::FixLink { note: "cat".into(), target: "Zzyzx".into(), to: Some("vet".into()) });
}

#[test]
fn consolidation_keeps_hand_written_frontmatter_and_leaves_session_notes_alone() {
    use cortex_core::memory::{NewNote, Proposal};
    let h = home("frontmatter");
    let mem = Memory::open(&h);
    std::fs::create_dir_all(mem.dir()).unwrap();
    std::fs::write(mem.dir().join("cat.md"), "---\ntitle: Cat\ntags: pets\n---\nBiscuit sees the [[Old vet]].\n").unwrap();
    add(&mem, "Vet", "Dr Lee at the corner clinic, the old vet.");
    let summary = "Summary: the user asked about the cat Biscuit and the old vet Dr Lee at the corner clinic.";
    mem.put("session-1", NewNote { title: "Session: cat".into(), body: summary.into(), kind: "session".into(), source: Some("1".into()) }).unwrap();

    let props = mem.consolidation();
    assert!(!props.iter().any(|p| matches!(p, Proposal::Merge { remove, keep, .. } | Proposal::Link { from: remove, to: keep } if remove.starts_with("session-") || keep.starts_with("session-"))),
        "session notes are summaries, not merged or linked: {props:?}");
    let fix = props.iter().find(|p| matches!(p, Proposal::FixLink { .. })).expect("a fix");
    mem.apply(fix, None).unwrap();
    let text = std::fs::read_to_string(mem.dir().join("cat.md")).unwrap();
    assert!(text.contains("tags: pets") && text.contains("[[vet]]"), "{text}");
}

#[test]
fn consolidation_proposes_at_most_ten_changes_at_a_time() {
    let mem = Memory::open(&home("cap"));
    for i in 0..15 { add(&mem, &format!("Note {i}"), &format!("Links to [[Missing {i}]].")); }
    assert_eq!(mem.consolidation().len(), 10);
}

#[test]
fn three_near_duplicates_are_merged_one_pair_per_review_so_every_proposal_still_applies() {
    use cortex_core::memory::Proposal;
    let mem = Memory::open(&home("merge3"));
    add(&mem, "Editor theme", "The user prefers dark mode in every editor.");
    add(&mem, "Editor colours", "The user prefers dark mode in every editor, always.");
    add(&mem, "Editor look", "The user prefers dark mode in every single editor.");
    let merges: Vec<Proposal> = mem.consolidation().into_iter().filter(|p| matches!(p, Proposal::Merge { .. })).collect();
    assert_eq!(merges.len(), 1, "{merges:?}");
    mem.apply(&merges[0], None).unwrap();
    let next: Vec<Proposal> = mem.consolidation().into_iter().filter(|p| matches!(p, Proposal::Merge { .. })).collect();
    assert_eq!(next.len(), 1, "the third note's turn comes next time: {next:?}");
    mem.apply(&next[0], None).unwrap();
    assert_eq!(mem.notes().len(), 1);
}

#[test]
fn a_merge_never_leaves_the_kept_note_linking_to_itself_or_a_link_twice() {
    use cortex_core::memory::{Candidate, Proposal};
    let mem = Memory::open(&home("merge_self"));
    let keep = add(&mem, "Dark theme", "The user prefers dark mode in every editor and terminal. Same as [[Dark mode]].");
    add(&mem, "Dark mode", "The user prefers dark mode in every editor.");
    let merge = Proposal::Merge { keep: keep.id.clone(), remove: "dark-mode".into(), body: keep.body.clone() };
    let kept = mem.apply(&merge, None).unwrap();
    assert!(!kept.body.contains("[["), "a link to the merged-away note isn't turned into a link to itself: {}", kept.body);
    // and related notes named twice are linked once
    let new = Proposal::New(Candidate { body: "Uses a dark terminal.".into(), kind: "fact".into(), related: vec!["Dark theme".into(), "Rust".into(), "dark-theme".into()] });
    assert_eq!(mem.apply(&new, None).unwrap().body.matches("[[dark-theme]]").count(), 1);
}

#[test]
fn many_dead_links_dont_crowd_out_merges_and_links() {
    use cortex_core::memory::Proposal;
    let mem = Memory::open(&home("cap_fair"));
    for i in 0..12 { add(&mem, &format!("Broken {i}"), &format!("Points at [[Qqq{i}]] zz{i}.")); }
    add(&mem, "Dark mode", "The user prefers dark mode in every editor.");
    add(&mem, "Dark theme", "The user prefers dark mode in every editor and terminal.");
    add(&mem, "Cat", "The cat Biscuit sleeps on the piano all day.");
    add(&mem, "Piano", "The piano in the living room is a Yamaha the cat sleeps on.");
    let props = mem.consolidation();
    assert_eq!(props.len(), 10);
    assert!(props.iter().any(|p| matches!(p, Proposal::Merge { .. })) && props.iter().any(|p| matches!(p, Proposal::Link { .. })), "{props:?}");
}

// ---------------------------------------------------------------- semantic re-ranking

use cortex_core::memory::Embedder;
use std::sync::Mutex;

/// Embeds by counting a few words ("feline" counts as "cat"), and records the texts of each call.
/// `works_for` calls succeed, then it fails (None: always works).
struct Fake { model: Option<String>, works_for: Option<usize>, calls: Mutex<Vec<Vec<String>>> }
impl Fake {
    fn new(model: &str) -> Fake { Fake { model: Some(model.into()), works_for: None, calls: Mutex::new(vec![]) } }
    fn without_model() -> Fake { Fake { model: None, ..Fake::new("") } }
    fn failing_after(calls: usize) -> Fake { Fake { works_for: Some(calls), ..Fake::new("nomic-embed-text") } }
    fn calls(&self) -> Vec<Vec<String>> { self.calls.lock().unwrap().clone() }
}
impl Embedder for Fake {
    fn model(&self) -> Option<String> { self.model.clone() }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(texts.to_vec());
        if self.works_for.is_some_and(|n| calls.len() > n) { return Err("the embedding model stopped".into()); }
        Ok(texts.iter().map(|t| {
            let t = t.to_lowercase();
            [&["cat", "feline"][..], &["shop"], &["pet"], &["food"]].iter().map(|ws| ws.iter().map(|w| t.matches(w).count() as f32).sum()).collect()
        }).collect())
    }
}

const ASK: &str = "tell me about my feline pet";

/// A cat note and a pet-shop note: by keywords the shop wins ("pet" four times), by meaning the cat.
fn cat_and_pet_shop(name: &str) -> (PathBuf, Memory) {
    let h = home(name);
    let mem = Memory::open(&h);
    add(&mem, "Cat", "Biscuit is the user's pet cat.");
    add(&mem, "Pet shop", "The pet shop sells pet food, pet beds and pet toys.");
    (h, mem)
}

fn ids(notes: Vec<cortex_core::memory::Note>) -> Vec<String> { notes.into_iter().map(|n| n.id).collect() }

#[test]
fn with_an_embedding_model_recall_puts_the_closest_meaning_first() {
    let (_, mem) = cat_and_pet_shop("sem_order");
    assert_eq!(ids(mem.recall(ASK, 8, 1000, None)), ["pet-shop", "cat"], "keywords alone");
    assert_eq!(ids(mem.recall(ASK, 8, 1000, Some(&Fake::new("nomic-embed-text")))), ["cat", "pet-shop"], "re-ranked by meaning");
    // with room for one note, the one that means the most gets it
    let one = mem.recall(ASK, 1, 1000, Some(&Fake::new("nomic-embed-text")));
    assert_eq!(ids(one), ["cat"]);
}

#[test]
fn without_an_embedding_model_recall_is_exactly_keyword_recall() {
    let (h, mem) = cat_and_pet_shop("sem_none");
    let none = Fake::without_model();
    assert_eq!(ids(mem.recall(ASK, 8, 1000, Some(&none))), ids(mem.recall(ASK, 8, 1000, None)));
    assert!(none.calls().is_empty(), "nothing is embedded");
    assert!(!h.join("memory").join(".embeddings.json").exists());
    // and an embedding model that fails partway falls back to keywords for that message
    let failing = Fake::failing_after(0);
    assert_eq!(ids(mem.recall(ASK, 8, 1000, Some(&failing))), ["pet-shop", "cat"]);
    // and nothing from the failed call is kept: once the model works, the notes are embedded
    let working = Fake::new("nomic-embed-text");
    assert_eq!(ids(mem.recall(ASK, 8, 1000, Some(&working))), ["cat", "pet-shop"]);
    assert_eq!(working.calls()[0].len(), 3);
}

#[test]
fn note_vectors_are_cached_and_made_again_only_when_needed() {
    let (h, mem) = cat_and_pet_shop("sem_cache");
    let cache = h.join("memory").join(".embeddings.json");
    let e = Fake::new("nomic-embed-text");
    mem.recall(ASK, 8, 1000, Some(&e));
    assert_eq!(e.calls()[0].len(), 3, "the message and both notes, in one call");
    assert!(cache.exists(), "kept beside the keyword index");
    mem.recall(ASK, 8, 1000, Some(&e));
    assert_eq!(e.calls()[1], [ASK.to_string()], "only the message: the notes come from the cache");

    // an edited note gets a new vector; the other doesn't
    std::fs::write(h.join("memory").join("cat.md"), "---\ntitle: Cat\n---\nBiscuit is the user's pet cat, a fluffy one.\n").unwrap();
    mem.recall(ASK, 8, 1000, Some(&e));
    assert_eq!(e.calls()[2].len(), 2, "the message and the edited note: {:?}", e.calls()[2]);
    assert!(e.calls()[2][1].contains("fluffy"));

    // a missing cache, a broken one, or another model's vectors: made again
    std::fs::remove_file(&cache).unwrap();
    mem.recall(ASK, 8, 1000, Some(&e));
    assert_eq!(e.calls()[3].len(), 3);
    std::fs::write(&cache, "not json").unwrap();
    mem.recall(ASK, 8, 1000, Some(&e));
    assert_eq!(e.calls()[4].len(), 3);
    let other = Fake::new("mxbai-embed-large");
    mem.recall(ASK, 8, 1000, Some(&other));
    assert_eq!(other.calls()[0].len(), 3, "vectors from another model can't be compared");
    mem.clear().unwrap();
    assert!(!cache.exists(), "clearing memory clears the vectors too");
}

#[test]
fn notes_are_embedded_in_batches_so_progress_survives_a_failure() {
    let mem = Memory::open(&home("sem_batch"));
    // cats and dogs alternate: keywords prefer the short dog notes, meaning the cat ones
    for i in 0..20 {
        let body = if i % 2 == 0 { format!("Pet number {i} is a cat, a cat called Biscuit{i} who sleeps all day.") } else { format!("Pet dog {i}.") };
        add(&mem, &format!("Pet {i}"), &body);
    }
    let flaky = Fake::failing_after(1);
    assert_eq!(ids(mem.recall(ASK, 8, 100_000, Some(&flaky))), ids(mem.recall(ASK, 8, 100_000, None)),
        "a failure partway means keywords alone, not an order from half the vectors");
    let calls = flaky.calls();
    assert_eq!(calls[0].len(), 16, "the message and 15 notes, then the rest in a second call");
    assert_eq!(calls[1].len(), 5);
    let working = Fake::new("nomic-embed-text");
    mem.recall(ASK, 8, 100_000, Some(&working));
    assert_eq!(working.calls(), [working.calls()[0].clone()]);
    assert_eq!(working.calls()[0].len(), 6, "the 15 kept from before aren't embedded again");
}

#[test]
fn vectors_of_deleted_notes_are_dropped_from_the_cache() {
    let (h, mem) = cat_and_pet_shop("sem_prune");
    add(&mem, "Dog", "Rex is the user's pet dog.");
    let e = Fake::new("nomic-embed-text");
    mem.recall(ASK, 8, 1000, Some(&e));
    std::fs::remove_file(h.join("memory").join("pet-shop.md")).unwrap();
    mem.recall(ASK, 8, 1000, Some(&e));
    let cache = std::fs::read_to_string(h.join("memory").join(".embeddings.json")).unwrap();
    assert!(cache.contains("cat.md") && !cache.contains("pet-shop.md"), "{cache}");
}

#[test]
fn with_nothing_to_rank_the_embedding_model_is_never_asked_for() {
    let h = home("sem_quiet");
    let mem = Memory::open(&h);
    let asked = std::cell::Cell::new(0);
    struct Counting<'a>(&'a std::cell::Cell<u32>);
    impl Embedder for Counting<'_> {
        fn model(&self) -> Option<String> { self.0.set(self.0.get() + 1); Some("nomic-embed-text".into()) }
        fn embed(&self, _: &[String]) -> Result<Vec<Vec<f32>>, String> { Err("not expected".into()) }
    }
    mem.recall(ASK, 8, 1000, Some(&Counting(&asked)));
    add(&mem, "Cat", "Biscuit is the user's pet cat.");
    mem.recall(ASK, 8, 1000, Some(&Counting(&asked)));
    assert_eq!(asked.get(), 0, "empty memory, then a single match: nothing to put in order, so no call to the server");
}
