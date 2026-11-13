//! `cortex memory ...` as a user runs it: the real binary, a separate Cortex home folder, and the
//! notes on disk and the command output checked.

use std::path::{Path, PathBuf};
use std::process::Command;

fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cortex_cli_mem_{name}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `cortex memory <args>`: (exit code, stdout, stderr).
fn cortex(home: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cortex")).arg("memory").args(args).env("CORTEX_HOME", home).output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

#[test]
fn memory_commands_list_show_search_and_follow_links() {
    let h = home("all");
    // an existing user's flat memory file, and a note written by hand that links to one of its facts
    std::fs::write(h.join("MEMORY.md"), "- Prefers dark mode in every editor\n- Works mostly in Rust\n").unwrap();
    let (code, out, _) = cortex(&h, &["list"]);
    assert_eq!(code, 0);
    assert!(out.contains("prefers-dark-mode-in-every-editor") && out.contains("works-mostly-in-rust"), "{out}");
    assert!(h.join("MEMORY.md.bak").exists() && !h.join("MEMORY.md").exists(), "migrated, with a backup");
    std::fs::write(h.join("memory").join("cortex.md"),
        "---\ntitle: Cortex project\nkind: project\n---\nA local agent, written in Rust: see [[Works mostly in Rust]].\n").unwrap();

    let (_, out, _) = cortex(&h, &["show", "Cortex project"]);
    assert!(out.contains("Cortex project") && out.contains("A local agent, written in Rust"), "{out}");
    assert!(out.contains("→ works-mostly-in-rust"), "{out}");

    let (_, out, _) = cortex(&h, &["links", "works-mostly-in-rust"]);
    assert!(out.contains("← cortex"), "linked from the hand-written note: {out}");

    let (_, out, _) = cortex(&h, &["search", "dark", "editor"]);
    assert!(out.lines().next().unwrap_or("").contains("prefers-dark-mode"), "{out}");
    let (_, out, _) = cortex(&h, &["search", "zebra"]);
    assert!(out.contains("nothing in memory matches"), "{out}");

    let (code, _, err) = cortex(&h, &["show", "no-such-note"]);
    assert_eq!(code, 1);
    assert!(err.contains("no note called"), "{err}");
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn an_empty_memory_says_so() {
    let h = home("empty");
    let (code, out, _) = cortex(&h, &["list"]);
    assert_eq!(code, 0);
    assert!(out.contains("no memories yet"), "{out}");
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn deleting_and_clearing_memory_from_the_command_line() {
    let h = home("delete");
    let dir = h.join("memory");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(h.join("sessions")).unwrap();
    std::fs::write(dir.join("cat.md"), "---\ntitle: Cat\nsource: 20261113-090000\n---\nThe cat is called Biscuit; see [[vet]].\n").unwrap();
    std::fs::write(dir.join("vet.md"), "---\ntitle: Vet\n---\nThe vet is Dr Lee.\n").unwrap();
    std::fs::write(dir.join("dog.md"), "---\ntitle: Dog\n---\nNo dog.\n").unwrap();

    // the session the cat note came from was cleared away
    let (_, out, _) = cortex(&h, &["show", "cat"]);
    assert!(out.contains("from session 20261113-090000 (deleted)"), "{out}");

    // nobody to ask (not a terminal) and no --yes: nothing is deleted
    let (code, _, _) = cortex(&h, &["delete", "vet"]);
    assert_eq!(code, 1);
    assert!(dir.join("vet.md").exists());

    let (code, out, _) = cortex(&h, &["delete", "vet", "--yes"]);
    assert_eq!(code, 0, "{out}");
    assert!(!dir.join("vet.md").exists());
    assert!(out.contains("cat"), "names the notes that still link to it: {out}");
    let (_, out, _) = cortex(&h, &["show", "cat"]);
    assert!(out.contains("→ vet  (no note called 'vet')"), "{out}");

    let (code, _, err) = cortex(&h, &["delete", "nobody", "--yes"]);
    assert_eq!(code, 1);
    assert!(err.contains("no note called"), "{err}");

    let (code, out, _) = cortex(&h, &["clear", "--yes"]);
    assert_eq!(code, 0);
    assert!(out.contains("deleted 2 notes"), "{out}");
    assert!(cortex(&h, &["list"]).1.contains("no memories yet"));
    let _ = std::fs::remove_dir_all(&h);
}

#[test]
fn clearing_sessions_says_memory_is_kept() {
    let h = home("sessions");
    std::fs::create_dir_all(h.join("sessions")).unwrap();
    std::fs::write(h.join("sessions").join("20261113-090000.json"), r#"{"id":"20261113-090000","model":"m","created":0,"messages":[]}"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cortex")).args(["sessions", "clear", "--yes"]).env("CORTEX_HOME", &h).output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("deleted 1 session") && out.contains("memory notes are kept"), "{out}");
    let _ = std::fs::remove_dir_all(&h);
}
