//! Long-term memory: a folder of Markdown notes, one fact or topic each, with frontmatter and
//! `[[wikilinks]]`. The files are the source of truth, so they can be read and edited in any text
//! editor. Search uses a BM25 index kept in `.index.json` beside them: a cache that is checked
//! against the files' modification times and rebuilt from them whenever it's missing or stale.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// The file name without .md; what `[[links]]` point to.
    pub id: String,
    pub title: String,
    /// fact, project, preference or session.
    pub kind: String,
    /// UTC, like 2026-11-12T15:30:00Z.
    pub created: String,
    pub updated: String,
    /// The session the note came from.
    pub source: Option<String>,
    pub body: String,
    pub path: PathBuf,
}

/// A note to save.
pub struct NewNote {
    pub title: String,
    pub body: String,
    pub kind: String,
    pub source: Option<String>,
}

pub enum Added {
    Created(Note),
    /// A note with the same text was there already.
    Exists(Note),
}

/// A `[[link]]` in a note, and the note it points to (None if there's no such note).
pub struct Link {
    pub target: String,
    pub note: Option<Note>,
}

pub struct Links {
    pub outgoing: Vec<Link>,
    /// Notes that link to this one.
    pub incoming: Vec<Note>,
}

pub struct Deleted {
    pub deleted: Note,
    /// Notes whose `[[links]]` pointed to it, and now point nowhere.
    pub linked_from: Vec<Note>,
}

pub struct Hit {
    pub note: Note,
    pub score: f64,
}

pub struct Memory {
    dir: PathBuf,
}

static WIKILINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\]|#]+)(?:[#|][^\]]*)?\]\]").expect("valid pattern"));

impl Memory {
    /// The memory folder in Cortex home folder `home`. An old flat MEMORY.md there is split into
    /// notes the first time, and kept as MEMORY.md.bak.
    pub fn open(home: &Path) -> Memory {
        let mem = Memory { dir: home.join("memory") };
        mem.migrate(&home.join("MEMORY.md"));
        mem
    }

    pub fn dir(&self) -> &Path { &self.dir }

    /// Every note, most recently updated first.
    pub fn notes(&self) -> Vec<Note> {
        let mut notes: Vec<Note> = self.files().iter().filter_map(|p| read_note(p)).collect();
        notes.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
        notes
    }

    /// The note with this id, or else this title (in any case).
    pub fn find(&self, id_or_title: &str) -> Option<Note> { lookup(&self.notes(), id_or_title).cloned() }

    /// Save a note, unless one with the same text exists already.
    pub fn add(&self, new: NewNote) -> Result<Added, String> {
        let body = new.body.trim();
        if body.is_empty() { return Err("a note needs some text".into()); }
        if let Some(n) = self.notes().into_iter().find(|n| n.body.trim().eq_ignore_ascii_case(body)) {
            return Ok(Added::Exists(n));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("couldn't create {}: {e}", self.dir.display()))?;
        // claim the file before writing it, so two Cortex windows saving at once can't pick the
        // same id and overwrite each other
        let base = slug(&new.title);
        let (id, path, mut file) = std::iter::once(base.clone()).chain((2..).map(|n| format!("{base}-{n}"))).find_map(|id| {
            let path = self.dir.join(format!("{id}.md"));
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => Some(Ok((id, path, f))),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(e) => Some(Err(format!("couldn't create {}: {e}", path.display()))),
            }
        }).expect("some id is free")?;
        let now = crate::agent::utc_iso(crate::agent::now());
        let title = one_line(&new.title);
        let mut text = format!("---\nid: {id}\ntitle: {title}\nkind: {}\ncreated: {now}\nupdated: {now}\n", one_line(&new.kind));
        if let Some(s) = &new.source { text += &format!("source: {}\n", one_line(s)); }
        text += &format!("---\n\n{body}\n");
        std::io::Write::write_all(&mut file, text.as_bytes()).map_err(|e| format!("couldn't write {}: {e}", path.display()))?;
        drop(file);
        read_note(&path).map(Added::Created).ok_or_else(|| format!("couldn't read back {}", path.display()))
    }

    /// Delete one note (by id or title). Links to it stay in other notes' text, now pointing
    /// nowhere; the result says which notes have them.
    pub fn delete(&self, id_or_title: &str) -> Result<Deleted, String> {
        let note = self.find(id_or_title).ok_or_else(|| format!("no note called '{}'", id_or_title.trim()))?;
        let linked_from = self.links(&note).incoming;
        std::fs::remove_file(&note.path).map_err(|e| format!("couldn't delete {}: {e}", note.path.display()))?;
        Ok(Deleted { deleted: note, linked_from })
    }

    /// Delete every note, and the index. Other files in the folder are left alone. Returns how many
    /// notes were deleted.
    pub fn clear(&self) -> Result<usize, String> {
        let files = self.files();
        for f in &files {
            std::fs::remove_file(f).map_err(|e| format!("couldn't delete {}: {e}", f.display()))?;
        }
        let _ = std::fs::remove_file(self.dir.join(".index.json"));
        Ok(files.len())
    }

    /// The links in a note, resolved, and the notes that link to it.
    pub fn links(&self, note: &Note) -> Links {
        let notes = self.notes();
        let resolve = |target: &str| lookup(&notes, target).cloned();
        let outgoing = wikilinks(&note.body).into_iter().map(|t| Link { note: resolve(&t), target: t }).collect();
        let incoming = notes.iter()
            .filter(|n| n.id != note.id && wikilinks(&n.body).iter().any(|t| resolve(t).is_some_and(|r| r.id == note.id)))
            .cloned().collect();
        Links { outgoing, incoming }
    }

    /// The notes best matching `query` (BM25 over titles and text), best first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let index = self.fresh_index();
        let terms: Vec<String> = tokens(query);
        if terms.is_empty() || index.docs.is_empty() { return vec![]; }
        let n = index.docs.len() as f64;
        let avg_len = index.docs.values().map(|d| d.length as f64).sum::<f64>() / n;
        let (k1, b) = (1.2, 0.75);
        // how rare each query word is across all notes
        let idf: HashMap<&String, f64> = terms.iter().map(|t| {
            let df = index.docs.values().filter(|d| d.terms.contains_key(t)).count() as f64;
            (t, (1.0 + (n - df + 0.5) / (df + 0.5)).ln())
        }).collect();
        let mut scored: Vec<(String, f64)> = index.docs.iter().filter_map(|(file, d)| {
            let score: f64 = terms.iter().map(|t| {
                let tf = *d.terms.get(t).unwrap_or(&0) as f64;
                if tf == 0.0 { return 0.0; }
                let idf = idf[t];
                idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * d.length as f64 / avg_len))
            }).sum();
            (score > 0.0).then(|| (file.clone(), score))
        }).collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.into_iter().take(limit)
            .filter_map(|(file, score)| read_note(&self.dir.join(file)).map(|note| Hit { note, score }))
            .collect()
    }

    /// The .md files in the folder (not hidden ones).
    fn files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.dir).into_iter().flatten().flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md"))
                && !p.file_name().and_then(|n| n.to_str()).unwrap_or(".").starts_with('.'))
            .collect();
        files.sort();
        files
    }

    /// The index, brought up to date with the files: notes whose modification time or size
    /// changed are read again, deleted ones dropped. A missing or unreadable index starts empty.
    fn fresh_index(&self) -> Index {
        let path = self.dir.join(".index.json");
        let mut index: Index = std::fs::read_to_string(&path).ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(|i: &Index| i.version == INDEX_VERSION)
            .unwrap_or_default();
        let mut changed = !path.exists();
        let mut seen = vec![];
        for file in self.files() {
            let Some(name) = file.file_name().and_then(|n| n.to_str()).map(String::from) else { continue };
            let stamp = std::fs::metadata(&file).ok().map(|m| (modified_ns(&m), m.len()));
            seen.push(name.clone());
            let current = index.docs.get(&name).is_some_and(|d| Some((d.modified_ns, d.size)) == stamp);
            if current { continue; }
            let (Some(note), Some((modified_ns, size))) = (read_note(&file), stamp) else { continue };
            // the title counts twice: it says what the note is about
            let words: Vec<String> = tokens(&format!("{} {} {}", note.title, note.title, note.body));
            let mut terms = HashMap::new();
            for w in &words { *terms.entry(w.clone()).or_insert(0u32) += 1; }
            index.docs.insert(name, Doc { modified_ns, size, length: words.len() as u32, terms });
            changed = true;
        }
        let before = index.docs.len();
        index.docs.retain(|name, _| seen.contains(name));
        changed |= index.docs.len() != before;
        if changed && self.dir.is_dir() {
            index.version = INDEX_VERSION;
            let _ = std::fs::write(&path, serde_json::to_string(&index).unwrap_or_default());
        }
        index
    }

    /// Split an old flat memory file (one fact per line) into notes, keeping it as a backup. One
    /// Cortex window claims the job with a lock file and moves the file to the backup first, so
    /// the facts are converted once, and a failure part-way never makes it run again (the backup
    /// has them all). Renaming alone can't be the claim: on Windows, windows that open the file
    /// at the same moment can all "succeed" in renaming it.
    fn migrate(&self, old: &Path) {
        if !old.exists() { return; }
        let name = old.file_name().and_then(|n| n.to_str()).unwrap_or("MEMORY.md");
        let lock = old.with_file_name(format!("{name}.migrating"));
        // a lock left by a window that crashed between taking it and moving the file
        let stale = std::fs::metadata(&lock).ok().and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 60);
        if stale { let _ = std::fs::remove_file(&lock); }
        if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() { return; } // another window has it
        let base = old.with_file_name(format!("{name}.bak"));
        let backup = PathBuf::from(crate::agent::first_free(&base.to_string_lossy(), |p| Path::new(p).exists()));
        let moved = std::fs::rename(old, &backup);
        let _ = std::fs::remove_file(&lock);
        if moved.is_err() { return; }
        let Ok(text) = std::fs::read_to_string(&backup) else { return };
        for line in text.lines() {
            let fact = line.trim().trim_start_matches(['-', '*']).trim();
            if fact.is_empty() || line.trim_start().starts_with('#') { continue; }
            let _ = self.add(NewNote { title: short_title(fact), body: fact.into(), kind: "fact".into(), source: None });
        }
    }
}

const INDEX_VERSION: u32 = 1;

#[derive(Default, Serialize, Deserialize)]
struct Index {
    version: u32,
    /// By file name.
    docs: HashMap<String, Doc>,
}

#[derive(Serialize, Deserialize)]
struct Doc {
    modified_ns: u64,
    size: u64,
    length: u32,
    terms: HashMap<String, u32>,
}

fn modified_ns(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Read a note file: frontmatter (`key: value` lines between `---`) and the body after it.
/// Missing fields fall back: the id to the file name, the title to the id, the kind to fact.
/// `updated` is the later of the frontmatter's date and the file's own time, so an edit made in a
/// text editor counts.
fn read_note(path: &Path) -> Option<Note> {
    let text = std::fs::read_to_string(path).ok()?.replace("\r\n", "\n");
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text); // Notepad's byte order mark
    let stem = path.file_stem()?.to_str()?.to_string();
    let lines: Vec<&str> = text.lines().collect();
    // frontmatter: from a first line of --- to the next line that is exactly ---
    let (front, body): (&[&str], &[&str]) = match lines.iter().skip(1).position(|l| l.trim_end() == "---") {
        Some(end) if lines.first().is_some_and(|l| l.trim_end() == "---") => (&lines[1..=end], &lines[end + 2..]),
        _ => (&[], &lines[..]),
    };
    let field = |key: &str| front.iter().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()))
        .filter(|v| !v.is_empty());
    let id = field("id").unwrap_or(stem);
    let file_time = std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| crate::agent::utc_iso(d.as_secs()));
    let updated = field("updated").into_iter().chain(file_time).max().unwrap_or_default();
    Some(Note {
        title: field("title").unwrap_or_else(|| id.clone()),
        kind: field("kind").unwrap_or_else(|| "fact".into()),
        created: field("created").unwrap_or_default(),
        updated,
        source: field("source"),
        body: body.join("\n").trim().to_string(),
        path: path.to_path_buf(),
        id,
    })
}

/// The targets of the `[[links]]` in a text, in order, each once.
fn wikilinks(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for c in WIKILINK.captures_iter(text) {
        let t = c[1].trim().to_string();
        if !t.is_empty() && !out.contains(&t) { out.push(t); }
    }
    out
}

/// Words for the index: lowercase letters and digits, without the most common words, and with a
/// plural "s" dropped so "themes" finds "theme".
fn tokens(text: &str) -> Vec<String> {
    const STOP: &[&str] = &["a", "an", "and", "are", "as", "at", "be", "by", "do", "does", "for", "from", "has", "have",
        "i", "in", "is", "it", "its", "me", "my", "of", "on", "or", "so", "that", "the", "this", "to", "was", "we",
        "were", "what", "when", "where", "which", "who", "why", "will", "with", "you", "your", "about"];
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty() && !STOP.contains(w))
        .map(|w| if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") { w[..w.len() - 1].to_string() } else { w.to_string() })
        .collect()
}

/// A file name for a title: lowercase words, in any language, joined by '-'.
fn slug(title: &str) -> String {
    let mut s = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_alphanumeric() { s.push(c); } else if !s.ends_with('-') && !s.is_empty() { s.push('-'); }
        if s.chars().count() >= 60 { break; }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "note".into() } else { s }
}

/// A title for a fact: its first words.
pub fn short_title(fact: &str) -> String {
    let words: Vec<&str> = fact.split_whitespace().take(8).collect();
    let mut title = words.join(" ");
    if title.len() > 60 { title = title.chars().take(57).collect::<String>() + "..."; }
    title.trim_end_matches(['.', ',', ';', ':']).to_string()
}

/// Text on one line, with runs of spaces and line breaks made single spaces.
pub fn one_line(s: &str) -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") }

/// The note with this id, or else this title (in any case).
fn lookup<'a>(notes: &'a [Note], key: &str) -> Option<&'a Note> {
    let key = key.trim();
    notes.iter().find(|n| n.id == key).or_else(|| notes.iter().find(|n| n.title.to_lowercase() == key.to_lowercase()))
}
