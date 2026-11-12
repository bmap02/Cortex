//! Models that aren't in the catalog: a GGUF on Hugging Face, pulled through Ollama's `hf.co/`
//! names, or a GGUF from any URL, downloaded here and handed to Ollama. Ollama only.
//!
//! A URL import uploads the file to Ollama as a blob and creates the model from it (Ollama's
//! `/api/blobs` + `/api/create` with `files`), rather than writing a Modelfile that points at a
//! local path: that's Ollama's current API, and it works when Ollama runs on another machine.

use crate::backend::{Backend, PullProgress};
use crate::cancel::CancelHandle;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

/// Where Hugging Face's API lives (tests point this at the fake server).
pub const HF_API: &str = "https://huggingface.co";
/// The quantization Ollama picks for an hf.co name without one.
const DEFAULT_QUANT: &str = "Q4_K_M";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A repo of GGUF files on Hugging Face, and which quantization to take.
    HuggingFace { repo: String, quant: String },
    /// A link to a .gguf file.
    Url(String),
}

/// A quantization in a GGUF file name: Q4_K_M, IQ3_XS, Q8_0, F16, BF16, ...
static QUANT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(?:^|[-_.])(i?q\d+(?:_[a-z0-9]+)*|bf16|f16|f32)(?:[-_.]|$)").expect("valid pattern"));
/// A parameter count in a file name: 7B, 1.5b, 70B.
static PARAMS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(?:^|[-_.])(\d+(?:\.\d+)?)b(?:[-_.]|$)").expect("valid pattern"));
/// A model name Ollama accepts: name, namespace/name, either with a :tag.
static MODEL_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*(/[A-Za-z0-9][A-Za-z0-9._-]*)?(:[A-Za-z0-9][A-Za-z0-9._-]*)?$").expect("valid pattern")
});

/// Whether Ollama accepts `name` as a model name (it also names a file here, so this keeps it to
/// letters, digits, `.`, `_` and `-`, with an optional namespace/ and :tag).
pub fn valid_model_name(name: &str) -> bool { MODEL_NAME.is_match(name) }

/// What the user typed: `hf.co/owner/repo[:QUANT]`, a huggingface.co repo link, `owner/repo`, or a
/// URL ending in .gguf. `quant` (from --quant) is used when the name doesn't give one.
pub fn parse_source(text: &str, quant: Option<&str>) -> Result<Source, String> {
    let text = text.trim();
    let is_url = text.starts_with("https://") || text.starts_with("http://");
    if is_url && file_name_of(text).to_lowercase().ends_with(".gguf") {
        // a file's page on Hugging Face (blob) is HTML; its download is the same path under resolve
        let url = if text.contains("huggingface.co/") { text.replacen("/blob/", "/resolve/", 1) } else { text.to_string() };
        return Ok(Source::Url(url));
    }
    let bare = text.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.");
    let repo_ref = bare.strip_prefix("hf.co/").or_else(|| bare.strip_prefix("huggingface.co/"));
    if is_url && repo_ref.is_none() {
        return Err(format!("{text} isn't a link to a .gguf file"));
    }
    let repo_ref = repo_ref.unwrap_or(bare).trim_end_matches('/');
    let (repo, named_quant) = match repo_ref.split_once(':') { Some((r, q)) => (r, Some(q)), None => (repo_ref, None) };
    let parts: Vec<&str> = repo.split('/').collect();
    // owner/repo, or a link to one of its pages (owner/repo/tree/main, ...)
    let page = parts.len() > 2 && ["tree", "blob", "resolve"].contains(&parts[2]);
    if !(parts.len() == 2 || page) || parts[..2].iter().any(|p| p.is_empty()) {
        return Err(format!("{text} isn't a Hugging Face repo (owner/name) or a .gguf link. \
            For a model from Ollama's library, use `cortex models install` or `cortex models pull`."));
    }
    let quant = named_quant.or(quant).unwrap_or(DEFAULT_QUANT).to_uppercase();
    Ok(Source::HuggingFace { repo: format!("{}/{}", parts[0], parts[1]), quant })
}

/// The last part of a URL's path, without its query.
fn file_name_of(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url).rsplit('/').next().unwrap_or("")
}

impl Source {
    /// The model's name in Ollama: hf.co/owner/repo:QUANT, or a name made from the file's.
    pub fn ollama_name(&self) -> String {
        match self {
            Source::HuggingFace { repo, quant } => format!("hf.co/{repo}:{quant}"),
            Source::Url(url) => {
                let file = file_name_of(url);
                let stem = file.get(..file.len().saturating_sub(5)).filter(|_| file.to_lowercase().ends_with(".gguf")).unwrap_or(file);
                let name: String = stem.to_lowercase().chars()
                    .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
                let name = name.trim_matches(['.', '-', '_']);
                if name.is_empty() { "imported-model".into() } else { name.into() }
            }
        }
    }
}

/// A file's size in GB guessed from its name: parameters × bits per weight of its quantization.
/// For when the server doesn't say; real files run a little bigger (some layers stay larger).
pub fn estimate_size_gb(file_name: &str) -> Option<f64> {
    let params: f64 = PARAMS.captures(file_name)?[1].parse().ok()?;
    let quant = QUANT.captures(file_name)?[1].to_uppercase();
    // typical bits per weight for llama.cpp's quantizations
    const BITS: [(&str, f64); 12] = [("IQ1", 1.75), ("IQ2", 2.5), ("Q2", 2.9), ("IQ3", 3.4), ("Q3", 3.9), ("IQ4", 4.4),
        ("Q4_0", 4.55), ("Q4", 4.85), ("Q5", 5.7), ("Q6", 6.6), ("Q8", 8.5), ("BF16", 16.0)];
    let bits = match quant.as_str() {
        "F16" => 16.0,
        "F32" => 32.0,
        q => BITS.iter().find(|(prefix, _)| q.starts_with(prefix)).map(|(_, b)| *b)?,
    };
    Some(params * bits / 8.0)
}

fn http() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(60))
        .user_agent(concat!("cortex/", env!("CARGO_PKG_VERSION"))).build()
}

/// Bytes in a repo's GGUF file(s) for `quant`: one file, or the parts of a split one added up.
pub fn hf_gguf_size(api_base: &str, repo: &str, quant: &str) -> Result<u64, String> {
    let url = format!("{}/api/models/{repo}/tree/main?recursive=true", api_base.trim_end_matches('/'));
    let files: Value = http().get(&url).call().map_err(|e| match e {
        ureq::Error::Status(404 | 401, _) => format!("{repo} isn't on Hugging Face (or it's private)"),
        e => format!("couldn't reach Hugging Face: {e}"),
    })?.into_json().map_err(|e| format!("Hugging Face sent something unexpected: {e}"))?;
    let ggufs: Vec<(String, u64)> = files.as_array().into_iter().flatten()
        .filter_map(|f| {
            let name = f["path"].as_str()?.rsplit('/').next()?.to_string();
            name.to_lowercase().ends_with(".gguf").then(|| (name, f["size"].as_u64().unwrap_or(0)))
        }).collect();
    let quant_of = |name: &str| QUANT.captures(name).map(|c| c[1].to_uppercase());
    let total: u64 = ggufs.iter().filter(|(n, _)| quant_of(n).as_deref() == Some(quant)).map(|(_, s)| s).sum();
    if total > 0 { return Ok(total); }
    let mut available: Vec<String> = vec![];
    for q in ggufs.iter().filter_map(|(n, _)| quant_of(n)) {
        if !available.contains(&q) { available.push(q); }
    }
    Err(if available.is_empty() {
        format!("{repo} has no GGUF files")
    } else {
        format!("{repo} has no {quant} file; it has {}", available.join(", "))
    })
}

/// A file's size from its server's headers (None if it doesn't say).
pub fn url_size(url: &str) -> Option<u64> {
    http().head(url).set("Accept-Encoding", "identity").call().ok()?.header("Content-Length")?.parse().ok()
}

/// Deletes the download when the import ends, however it ends: Ollama keeps its own copy.
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
}

/// Download the GGUF at `url` into `work_dir`, upload it to Ollama and create model `name` from
/// it. `on_progress` gets each step; `cancel` stops it, and a stopped import leaves no file and
/// no model behind (the model is only created once the whole file is in Ollama).
pub fn import_from_url(backend: &Backend, url: &str, name: &str, work_dir: &Path, cancel: &CancelHandle,
                       on_progress: &mut dyn FnMut(&PullProgress)) -> Result<(), String> {
    if !valid_model_name(name) { return Err(format!("'{name}' isn't a name Ollama accepts (letters, digits, . _ -)")); }
    std::fs::create_dir_all(work_dir).map_err(|e| format!("couldn't create {}: {e}", work_dir.display()))?;
    // named by this process, not the model, so no name can point outside the folder
    let file = TempFile(work_dir.join(format!("import-{}.gguf.part", std::process::id())));
    let (digest, size) = download(url, &file.0, cancel, on_progress)?;
    backend.upload_blob(&file.0, &digest, size, cancel, on_progress).map_err(|e| e.to_string())?;
    drop(file);
    backend.create_model(name, "model.gguf", &digest, on_progress).map_err(|e| e.to_string())
}

/// Stream `url` into `dest`, hashing as it goes. Returns the sha256 and the size.
fn download(url: &str, dest: &Path, cancel: &CancelHandle, on_progress: &mut dyn FnMut(&PullProgress)) -> Result<(String, u64), String> {
    // no compression: Content-Length must count the bytes we receive
    let resp = http().get(url).set("Accept-Encoding", "identity").call().map_err(|e| format!("couldn't download {url}: {e}"))?;
    let total: u64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let status = format!("downloading {}", file_name_of(url));
    let mut reader = resp.into_reader();
    let mut out = std::io::BufWriter::new(std::fs::File::create(dest).map_err(|e| format!("couldn't write {}: {e}", dest.display()))?);
    let (mut hash, mut done, mut buf) = (Sha256::new(), 0u64, vec![0u8; 256 * 1024]);
    loop {
        if cancel.is_cancelled() { return Err("import stopped".into()); }
        let n = reader.read(&mut buf).map_err(|e| format!("the download broke off: {e}"))?;
        if n == 0 { break; }
        out.write_all(&buf[..n]).map_err(|e| format!("couldn't write {}: {e}", dest.display()))?;
        hash.update(&buf[..n]);
        done += n as u64;
        on_progress(&PullProgress { status: status.clone(), completed: done, total: total.max(done) });
    }
    out.flush().map_err(|e| format!("couldn't write {}: {e}", dest.display()))?;
    if total > 0 && done != total { return Err(format!("the download ended early ({done} of {total} bytes)")); }
    Ok((hash.finalize().iter().map(|b| format!("{b:02x}")).collect(), done))
}

#[cfg(test)]
mod tests {
    use super::QUANT;

    fn quant(name: &str) -> Option<String> { QUANT.captures(name).map(|c| c[1].to_uppercase()) }

    #[test]
    fn finds_the_quantization_in_file_names() {
        assert_eq!(quant("Qwen2.5-7B-Instruct-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
        assert_eq!(quant("model.IQ3_XS.gguf").as_deref(), Some("IQ3_XS"));
        assert_eq!(quant("Qwen2.5-7B-Instruct-f16-00001-of-00002.gguf").as_deref(), Some("F16"));
        assert_eq!(quant("Qwen2.5-7B-Instruct.gguf"), None, "the 7B isn't a quantization");
    }
}
