//! Long-term memory: a folder of Markdown notes, one fact or topic each, with frontmatter and
//! `[[wikilinks]]`. The files are the source of truth, so they can be read and edited in any text
//! editor. Search uses a BM25 index kept in `.index.json` beside them: a cache that is checked
//! against the files' modification times and rebuilt from them whenever it's missing or stale.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

/// A memory the agent proposed during a session, held until the user reviews it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub body: String,
    /// fact, preference or project.
    pub kind: String,
    /// Notes it relates to (ids or titles): it links to those that exist when it's saved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<String>,
}

/// A change to memory that waits for the user's OK. Notes are named by id.
#[derive(Debug, Clone, PartialEq)]
pub enum Proposal {
    /// Save a candidate as a new note.
    New(Candidate),
    /// Fold note `remove` into note `keep`, whose text becomes `body`; links to `remove` move to `keep`.
    Merge { keep: String, remove: String, body: String },
    /// Add a link from note `from` to note `to`.
    Link { from: String, to: String },
    /// Note `note` links to `target`, which doesn't exist: point the link at note `to`, or (None)
    /// turn it into plain text.
    FixLink { note: String, target: String, to: Option<String> },
}

impl Proposal {
    /// The part the user can edit: a note's text, or the note a link points to.
    pub fn text(&self) -> String {
        match self {
            Proposal::New(c) => c.body.clone(),
            Proposal::Merge { body, .. } => body.clone(),
            Proposal::Link { to, .. } => to.clone(),
            Proposal::FixLink { to, .. } => to.clone().unwrap_or_default(),
        }
    }

    /// The proposal with its editable part replaced by `text` (an empty target removes a dead link).
    pub fn with_text(&self, text: &str) -> Proposal {
        let t = text.trim().to_string();
        match self.clone() {
            Proposal::New(c) => Proposal::New(Candidate { body: t, ..c }),
            Proposal::Merge { keep, remove, .. } => Proposal::Merge { keep, remove, body: t },
            Proposal::Link { from, .. } => Proposal::Link { from, to: t },
            Proposal::FixLink { note, target, .. } => Proposal::FixLink { note, target, to: (!t.is_empty()).then_some(t) },
        }
    }
}

/// Turns text into vectors whose closeness follows meaning (an embedding model). Optional: recall
/// re-ranks its keyword hits with one when there is one.
pub trait Embedder {
    /// The model that makes the vectors, or None when there isn't one. Vectors from different
    /// models can't be compared, so the cache is kept per model.
    fn model(&self) -> Option<String>;
    /// One vector per text, in order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
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
        std::io::Write::write_all(&mut file, note_text(&id, &new, &now, &now).as_bytes()).map_err(|e| format!("couldn't write {}: {e}", path.display()))?;
        drop(file);
        read_note(&path).map(Added::Created).ok_or_else(|| format!("couldn't read back {}", path.display()))
    }

    /// Write the note with this id, replacing it if it's there (it keeps its created date). For
    /// notes Cortex rewrites, like a session's summary. `id` must be a file-name-safe slug.
    pub fn put(&self, id: &str, new: NewNote) -> Result<Note, String> {
        if new.body.trim().is_empty() { return Err("a note needs some text".into()); }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("couldn't create {}: {e}", self.dir.display()))?;
        let path = self.dir.join(format!("{id}.md"));
        let now = crate::agent::utc_iso(crate::agent::now());
        let created = read_note(&path).map(|n| n.created).filter(|c| !c.is_empty()).unwrap_or_else(|| now.clone());
        self.write_whole(&path, &note_text(id, &new, &created, &now))?;
        read_note(&path).ok_or_else(|| format!("couldn't read back {}", path.display()))
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
        let _ = std::fs::remove_file(self.dir.join(".embeddings.json"));
        Ok(files.len())
    }

    /// What could be tidied, for the user to review: dead links (each pointed at the closest note,
    /// or made plain text when nothing is close), then notes so alike they should be one, then
    /// notes alike enough to link. At most ten at a time, so a review stays short. Session notes
    /// are summaries: they're never merged or linked, only their dead links fixed.
    pub fn consolidation(&self) -> Vec<Proposal> {
        const MAX: usize = 10;
        const MERGE_AT: f64 = 0.6; // share of words in common
        const LINK_AT: f64 = 0.25;
        let all = self.notes();
        let mut fixes = vec![];
        for n in &all {
            for target in wikilinks(&n.body).into_iter().filter(|t| lookup(&all, t).is_none()) {
                let to = self.search(&target, 5).into_iter().map(|h| h.note).find(|h| h.id != n.id && h.kind != "session").map(|h| h.id);
                fixes.push(Proposal::FixLink { note: n.id.clone(), target, to });
            }
        }
        let words: Vec<(&Note, HashSet<String>)> = all.iter().filter(|n| n.kind != "session")
            .map(|n| (n, tokens(&format!("{} {}", n.title, n.body)).into_iter().collect())).collect();
        let (mut merges, mut links) = (vec![], vec![]);
        for (i, (a, wa)) in words.iter().enumerate() {
            for (b, wb) in &words[i + 1..] {
                let union = wa.union(wb).count();
                let similar = if union == 0 { 0.0 } else { wa.intersection(wb).count() as f64 / union as f64 };
                if similar >= MERGE_AT { merges.push((similar, *a, *b)); }
                else if similar >= LINK_AT && !linked(&all, a, b) { links.push((similar, *a, *b)); }
            }
        }
        merges.sort_by(|x, y| y.0.total_cmp(&x.0));
        links.sort_by(|x, y| y.0.total_cmp(&x.0));
        let mut merged: Vec<&str> = vec![]; // a note goes into at most one merge per review
        let mut merge_proposals = vec![];
        for (_, a, b) in merges {
            if merged.contains(&a.id.as_str()) || merged.contains(&b.id.as_str()) { continue; }
            // keep the fuller note (or the older one), and add the lines only the other has
            let (keep, remove) = if (b.body.len(), &a.created) > (a.body.len(), &b.created) { (b, a) } else { (a, b) };
            let mut body = keep.body.clone();
            for line in remove.body.lines().map(str::trim).filter(|l| !l.is_empty() && !keep.body.contains(l)) { body += &format!("\n{line}"); }
            merged.extend([a.id.as_str(), b.id.as_str()]);
            merge_proposals.push(Proposal::Merge { keep: keep.id.clone(), remove: remove.id.clone(), body });
        }
        // notes are newest first: the newer note links to the older one
        let link_proposals = links.into_iter().map(|(_, a, b)| Proposal::Link { from: a.id.clone(), to: b.id.clone() });
        // one of each kind in turn, so many dead links don't crowd out merges and links
        let mut kinds = [fixes.into_iter(), merge_proposals.into_iter(), link_proposals.collect::<Vec<_>>().into_iter()];
        let mut out = vec![];
        loop {
            let before = out.len();
            for kind in kinds.iter_mut() {
                if out.len() < MAX && let Some(p) = kind.next() { out.push(p); }
            }
            if out.len() == before { return out; }
        }
    }

    /// Carry out an approved proposal; `source` (a session id) is recorded on a new note. Returns
    /// the note that was created or changed (for a merge, the one kept).
    pub fn apply(&self, p: &Proposal, source: Option<&str>) -> Result<Note, String> {
        let all = self.notes();
        let get = |key: &str| lookup(&all, key).cloned().ok_or_else(|| format!("no note called '{}'", key.trim()));
        let reread = |id: &str| self.find(id).ok_or_else(|| format!("couldn't read back {id}"));
        match p {
            Proposal::New(c) => {
                let mut seen = HashSet::new();
                let ids: Vec<String> = c.related.iter().filter_map(|r| lookup(&all, r)).map(|n| n.id.clone()).filter(|id| seen.insert(id.clone())).collect();
                let mut body = c.body.trim().to_string();
                if !ids.is_empty() { body += &format!("\n\nRelated: {}", ids.iter().map(|i| format!("[[{i}]]")).collect::<Vec<_>>().join(" ")); }
                match self.add(NewNote { title: short_title(&c.body), body, kind: c.kind.clone(), source: source.map(String::from) })? {
                    Added::Created(n) | Added::Exists(n) => Ok(n),
                }
            }
            Proposal::Merge { keep, remove, body } => {
                let (keep, remove) = (get(keep)?, get(remove)?);
                if keep.id == remove.id { return Err("a note can't be merged into itself".into()); }
                if body.trim().is_empty() { return Err("a note needs some text".into()); }
                self.rewrite(&keep, |_| body.trim().to_string())?;
                // links to the removed note now point at the kept one
                let all = self.notes();
                for n in all.iter().filter(|n| n.id != remove.id) {
                    // the kept note's own link to the other would point at itself: make it plain text
                    let to = (n.id != keep.id).then_some(keep.id.as_str());
                    self.rewrite_links(n, |t| lookup(&all, t).is_some_and(|r| r.id == remove.id), to)?;
                }
                std::fs::remove_file(&remove.path).map_err(|e| format!("couldn't delete {}: {e}", remove.path.display()))?;
                reread(&keep.id)
            }
            Proposal::Link { from, to } => {
                let (from, to) = (get(from)?, get(to)?);
                if from.id == to.id { return Err("a note can't link to itself".into()); }
                if !links_among(&all, &from).outgoing.iter().any(|l| l.note.as_ref().is_some_and(|n| n.id == to.id)) {
                    self.rewrite(&from, |body| {
                        let link = format!("[[{}]]", to.id);
                        // add to a "See also:" line already there, or start one
                        match body.lines().position(|l| l.starts_with("See also:")) {
                            Some(i) => body.lines().enumerate().map(|(j, l)| if j == i { format!("{l} {link}") } else { l.to_string() }).collect::<Vec<_>>().join("\n"),
                            None => format!("{body}\n\nSee also: {link}"),
                        }
                    })?;
                }
                reread(&from.id)
            }
            Proposal::FixLink { note, target, to } => {
                let note = get(note)?;
                let to = to.as_deref().map(get).transpose()?;
                self.rewrite_links(&note, |t| t == target.trim(), to.as_ref().map(|n| n.id.as_str()))?;
                reread(&note.id)
            }
        }
    }

    /// Replace a note's text with `f(text)`, keeping its frontmatter exactly as written (a note
    /// written by hand may have fields Cortex doesn't know).
    fn rewrite(&self, note: &Note, f: impl FnOnce(&str) -> String) -> Result<(), String> {
        let raw = std::fs::read_to_string(&note.path).map_err(|e| format!("couldn't read {}: {e}", note.path.display()))?.replace("\r\n", "\n");
        let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let (front, body) = split_frontmatter(raw);
        let body = f(body.trim());
        let text = if front.is_empty() { format!("{}\n", body.trim()) } else { format!("{front}\n{}\n", body.trim()) };
        self.write_whole(&note.path, &text)
    }

    /// Point the links in `note` whose target `matches` at note `to`, or (None) make them plain text.
    fn rewrite_links(&self, note: &Note, matches: impl Fn(&str) -> bool, to: Option<&str>) -> Result<(), String> {
        if !wikilinks(&note.body).iter().any(|t| matches(t)) { return Ok(()); }
        self.rewrite(note, |body| WIKILINK.replace_all(body, |c: &regex::Captures| {
            let target = c[1].trim();
            match (matches(target), to) {
                (false, _) => c[0].to_string(),
                (true, Some(id)) => format!("[[{id}]]"),
                (true, None) => target.to_string(),
            }
        }).into_owned())
    }

    /// Write a file whole, then swap it in: a crash never leaves half a note.
    fn write_whole(&self, path: &Path, text: &str) -> Result<(), String> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("note.md");
        let tmp = self.dir.join(format!(".{name}.{}.tmp", std::process::id())); // two windows never share one
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("couldn't write {}: {e}", path.display()) })
    }

    /// The links in a note, resolved, and the notes that link to it.
    pub fn links(&self, note: &Note) -> Links { links_among(&self.notes(), note) }

    /// Whether there are no notes at all.
    pub fn is_empty(&self) -> bool { self.files().is_empty() }

    /// The notes to bring into a conversation about `query`: the best `hits` matches, then the
    /// notes one link away from them (in either direction), most relevant first, cut to
    /// `budget_tokens` as rendered by `render_recalled`. A note that doesn't fit is skipped for a
    /// smaller one after it; dead links (to deleted notes) are skipped too.
    ///
    /// The matches are found by keywords. With an `embedder` that has a model, a wider set of
    /// keyword matches is put in order of how close each note's meaning is to `query`, and the
    /// best `hits` of those are taken; if embedding fails, keywords alone decide, as without one.
    pub fn recall(&self, query: &str, hits: usize, budget_tokens: usize, embedder: Option<&dyn Embedder>) -> Vec<Note> {
        let mut found: Vec<Note> = self.search(query, hits * 3).into_iter().map(|h| h.note).collect();
        // the server is only asked about embeddings when there's more than one note to put in order
        if found.len() > 1 && let Some(embedder) = embedder && let Some(model) = embedder.model()
            && let Some(ordered) = self.by_meaning(query, &found, embedder, &model) {
            found = ordered;
        }
        found.truncate(hits);
        let hits = found;
        if hits.is_empty() { return vec![]; }
        let all = self.notes(); // read once for every hit's links
        let mut candidates: Vec<Note> = hits.clone();
        for hit in &hits {
            let links = links_among(&all, hit);
            candidates.extend(links.outgoing.into_iter().filter_map(|l| l.note));
            candidates.extend(links.incoming);
        }
        let (mut picked, mut used): (Vec<Note>, usize) = (vec![], 0);
        for note in candidates {
            if picked.iter().any(|p| p.id == note.id) { continue; }
            let cost = recall_tokens(&note);
            if used + cost > budget_tokens { continue; }
            used += cost;
            picked.push(note);
        }
        picked
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

    /// `notes` in order of how close their meaning is to `query` (ties keep their order), or None
    /// if embedding fails. Note vectors are cached in `.embeddings.json`, per model, and made again
    /// only for notes whose file changed since; a missing or unreadable cache starts empty. Texts
    /// go in batches, and each batch's vectors are kept even if a later one fails, so a slow
    /// model gets through a large memory over several messages instead of failing it every time.
    fn by_meaning(&self, query: &str, notes: &[Note], embedder: &dyn Embedder, model: &str) -> Option<Vec<Note>> {
        const BATCH: usize = 16;
        let path = self.dir.join(".embeddings.json");
        let mut cache: Vectors = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok())
            .filter(|c: &Vectors| c.model == model)
            .unwrap_or_else(|| Vectors { model: model.to_string(), docs: HashMap::new() });
        let stale: Vec<&Note> = notes.iter()
            .filter(|n| !cache.docs.get(file_name(&n.path)).is_some_and(|v| Some(v.stamp) == stamp(&n.path))).collect();
        let mut texts = vec![query.to_string()];
        texts.extend(stale.iter().map(|n| format!("{}\n{}", n.title, n.body)));
        let mut vectors: Vec<Vec<f32>> = vec![];
        for batch in texts.chunks(BATCH) {
            match embedder.embed(batch) {
                Ok(v) if v.len() == batch.len() => vectors.extend(v),
                _ => break,
            }
        }
        // vectors[0] is the message's; the rest belong to the stale notes, in order
        for (n, v) in stale.iter().zip(vectors.iter().skip(1)) {
            if let Some(stamp) = stamp(&n.path) { cache.docs.insert(file_name(&n.path).to_string(), Vector { stamp, vector: v.clone() }); }
        }
        let names: HashSet<String> = self.files().iter().map(|p| file_name(p).to_string()).collect();
        let before = cache.docs.len();
        cache.docs.retain(|f, _| names.contains(f)); // notes deleted since
        if (vectors.len() > 1 || cache.docs.len() != before) && self.dir.is_dir() {
            let _ = self.write_whole(&path, &serde_json::to_string(&cache).unwrap_or_default());
        }
        if vectors.len() != texts.len() { return None; }
        let score = |n: &Note| cache.docs.get(file_name(&n.path)).map_or(f32::MIN, |v| cosine(&vectors[0], &v.vector));
        let mut ordered = notes.to_vec();
        ordered.sort_by(|a, b| score(b).total_cmp(&score(a)));
        Some(ordered)
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
            let stamp = stamp(&file);
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

/// The note vectors one embedding model made, by file name.
#[derive(Serialize, Deserialize)]
struct Vectors {
    model: String,
    docs: HashMap<String, Vector>,
}

/// A note's vector, and the file's (modification time, size) when it was made.
#[derive(Serialize, Deserialize)]
struct Vector {
    stamp: (u64, u64),
    vector: Vec<f32>,
}

/// A file's modification time (ns) and size: whether a cache built from it is still current.
fn stamp(path: &Path) -> Option<(u64, u64)> { std::fs::metadata(path).ok().map(|m| (modified_ns(&m), m.len())) }

fn file_name(path: &Path) -> &str { path.file_name().and_then(|f| f.to_str()).unwrap_or_default() }

/// How alike two vectors point: 1 the same direction, 0 unrelated.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let n = norm(a) * norm(b);
    if n == 0.0 || a.len() != b.len() { 0.0 } else { dot / n }
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

/// A note file: frontmatter, then the text.
fn note_text(id: &str, new: &NewNote, created: &str, updated: &str) -> String {
    let mut text = format!("---\nid: {id}\ntitle: {}\nkind: {}\ncreated: {created}\nupdated: {updated}\n", one_line(&new.title), one_line(&new.kind));
    if let Some(s) = &new.source { text += &format!("source: {}\n", one_line(s)); }
    text + &format!("---\n\n{}\n", new.body.trim())
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
        if c == '\'' || c == '’' { continue; } // "user's" → "users", not "user-s"
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

/// Whether either note links to the other.
fn linked(notes: &[Note], a: &Note, b: &Note) -> bool {
    let to = |from: &Note, to: &Note| wikilinks(&from.body).iter().any(|t| lookup(notes, t).is_some_and(|n| n.id == to.id));
    to(a, b) || to(b, a)
}

/// A note file's frontmatter (from the first `---` line through the next, newline included; empty
/// if there's none) and the text after it.
fn split_frontmatter(text: &str) -> (&str, &str) {
    if !text.starts_with("---\n") { return ("", text); }
    let mut at = 4;
    for line in text[4..].split_inclusive('\n') {
        at += line.len();
        if line.trim_end() == "---" { return text.split_at(at); }
    }
    ("", text)
}

/// A note's links resolved among `notes`, and the notes there that link to it.
fn links_among(notes: &[Note], note: &Note) -> Links {
    let resolve = |target: &str| lookup(notes, target).cloned();
    let outgoing = wikilinks(&note.body).into_iter().map(|t| Link { note: resolve(&t), target: t }).collect();
    let incoming = notes.iter()
        .filter(|n| n.id != note.id && wikilinks(&n.body).iter().any(|t| resolve(t).is_some_and(|r| r.id == note.id)))
        .cloned().collect();
    Links { outgoing, incoming }
}

/// How a recalled note is shown to the model: its title, id (for `[[links]]`) and text.
fn render_note(n: &Note) -> String { format!("## {} [[{}]]\n{}\n", n.title, n.id, n.body) }

/// Roughly how many tokens a recalled note takes in `render_recalled` (4 characters a token, as
/// for the context), counting the line that separates it from the next.
pub fn recall_tokens(n: &Note) -> usize { (render_note(n).len() + 1).div_ceil(4) }

/// Recalled notes as one block of text for the model.
pub fn render_recalled(notes: &[Note]) -> String { notes.iter().map(render_note).collect::<Vec<_>>().join("\n") }

/// A note as the `recall` tool returns it: the note, then which notes it links to and from.
pub fn render_with_links(n: &Note, links: &Links) -> String {
    let mut s = render_note(n);
    let out: Vec<&str> = links.outgoing.iter().filter_map(|l| l.note.as_ref().map(|t| t.id.as_str())).collect();
    let inc: Vec<&str> = links.incoming.iter().map(|t| t.id.as_str()).collect();
    if !out.is_empty() { s += &format!("links to: {}\n", out.join(", ")); }
    if !inc.is_empty() { s += &format!("linked from: {}\n", inc.join(", ")); }
    s
}

/// A note's text without the "Related:" links added when a suggested memory was saved: the fact
/// as it was suggested.
pub fn fact_of(body: &str) -> &str { body.split("\n\nRelated: ").next().unwrap_or(body).trim() }

/// Text on one line, with runs of spaces and line breaks made single spaces.
pub fn one_line(s: &str) -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") }

/// The note with this id, or else this title (in any case).
fn lookup<'a>(notes: &'a [Note], key: &str) -> Option<&'a Note> {
    let key = key.trim();
    notes.iter().find(|n| n.id == key).or_else(|| notes.iter().find(|n| n.title.to_lowercase() == key.to_lowercase()))
}
