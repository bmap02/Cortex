//! Importing models that aren't in the catalog: a Hugging Face GGUF through Ollama's hf.co names,
//! or a GGUF from any URL, downloaded, uploaded to Ollama and created there.

use cortex_core::backend::{Backend, PullProgress};
use cortex_core::cancel::CancelHandle;
use cortex_core::config::Config;
use cortex_core::import::{estimate_size_gb, hf_gguf_size, import_from_url, parse_source, url_size, valid_model_name, Source};

mod common;
use common::MockServer;

const REPO: &str = "bartowski/Qwen2.5-7B-Instruct-GGUF";

fn hf(repo: &str, quant: &str) -> Source { Source::HuggingFace { repo: repo.into(), quant: quant.into() } }

#[test]
fn reads_hugging_face_repos_and_gguf_urls() {
    assert_eq!(parse_source("hf.co/bartowski/Qwen2.5-7B-Instruct-GGUF:Q8_0", None).unwrap(), hf(REPO, "Q8_0"));
    assert_eq!(parse_source("https://huggingface.co/bartowski/Qwen2.5-7B-Instruct-GGUF", Some("q5_k_m")).unwrap(), hf(REPO, "Q5_K_M"));
    assert_eq!(parse_source("bartowski/Qwen2.5-7B-Instruct-GGUF", None).unwrap(), hf(REPO, "Q4_K_M"), "Ollama's default quantization");
    let url = "https://example.com/models/My-Model.Q4_K_M.gguf";
    assert_eq!(parse_source(url, None).unwrap(), Source::Url(url.into()));
    // a direct link to a file on Hugging Face is a URL import too
    let file = "https://huggingface.co/bartowski/x-GGUF/resolve/main/x-Q4_K_M.gguf";
    assert_eq!(parse_source(file, None).unwrap(), Source::Url(file.into()));
    // links to a repo's pages, and a file's page (blob) rather than its download (resolve)
    assert_eq!(parse_source("https://huggingface.co/bartowski/Qwen2.5-7B-Instruct-GGUF/tree/main", None).unwrap(), hf(REPO, "Q4_K_M"));
    assert_eq!(parse_source("https://huggingface.co/o/r-GGUF/blob/main/r-Q8_0.gguf", None).unwrap(),
        Source::Url("https://huggingface.co/o/r-GGUF/resolve/main/r-Q8_0.gguf".into()));
    assert!(parse_source("qwen2.5:7b", None).unwrap_err().contains("models install"), "a library model");
    assert!(parse_source("https://example.com/model.bin", None).is_err(), "not a GGUF");
}

#[test]
fn names_the_model_in_ollama() {
    assert_eq!(hf(REPO, "Q4_K_M").ollama_name(), "hf.co/bartowski/Qwen2.5-7B-Instruct-GGUF:Q4_K_M");
    assert_eq!(Source::Url("https://x.org/a/My-Model.Q4_K_M.gguf?download=1".into()).ollama_name(), "my-model.q4_k_m");
    assert_eq!(Source::Url("https://x.org/a/.gguf".into()).ollama_name(), "imported-model");
    assert_eq!(Source::Url("https://x.org/a/..hidden Model.gguf".into()).ollama_name(), "hidden-model");
}

#[test]
fn only_names_ollama_accepts_are_used() {
    for ok in ["my-model", "me/my-model", "my-model:q4", "qwen2.5-7b"] { assert!(valid_model_name(ok), "{ok}"); }
    for bad in ["", "../x", "a/b/c", "My Model", ".hidden", "x:", r"C:\x"] { assert!(!valid_model_name(bad), "{bad}"); }
}

#[test]
fn guesses_a_files_size_from_its_name_when_the_server_does_not_say() {
    // parameters × bits per weight for the quantization
    let gb = |name: &str| estimate_size_gb(name).map(|g| (g * 10.0).round() / 10.0);
    assert_eq!(gb("Qwen2.5-7B-Instruct-Q4_K_M.gguf"), Some(4.2)); // the real file is 4.7 (embeddings stay larger)
    assert_eq!(gb("Llama-3.1-70B-Instruct.Q8_0.gguf"), Some(74.4));
    assert_eq!(gb("model-1.5b-f16.gguf"), Some(3.0));
    assert_eq!(gb("model-Q4_K_M.gguf"), None, "no parameter count");
    assert_eq!(gb("Qwen2.5-7B-Instruct.gguf"), None, "no quantization");
}

#[test]
fn finds_the_size_of_a_hugging_face_gguf() {
    let server = MockServer::start();
    let api = server.url();
    assert_eq!(hf_gguf_size(&api, REPO, "Q4_K_M").unwrap(), 4_683_074_240);
    // big models come split into parts in a folder: their sizes add up
    assert_eq!(hf_gguf_size(&api, REPO, "F16").unwrap(), 15_200_000_000);
    let missing = hf_gguf_size(&api, REPO, "Q3_K_S").unwrap_err();
    assert!(missing.contains("Q4_K_M") && missing.contains("Q8_0"), "says which quantizations there are: {missing}");
    assert!(hf_gguf_size(&api, "nobody/nothing", "Q4_K_M").is_err());
}

#[test]
fn a_gguf_url_is_downloaded_uploaded_and_created_in_ollama() {
    let server = MockServer::start();
    let be = Backend::new(&Config { base_url: server.url(), ..Config::default() });
    let url = format!("{}/files/model.gguf", server.url());
    let size = url_size(&url).expect("the server says how big it is");
    assert!(size > 1_000_000);

    let work = std::env::temp_dir().join(format!("cortex_import_ok{}", std::process::id()));
    let mut stages: Vec<String> = vec![];
    import_from_url(&be, &url, "my-model", &work, &CancelHandle::new(), &mut |p: &PullProgress| {
        if stages.last() != Some(&p.status) { stages.push(p.status.clone()); }
    }).unwrap();

    assert!(stages.iter().any(|s| s.starts_with("downloading")), "{stages:?}");
    assert!(stages.iter().any(|s| s.starts_with("uploading")), "{stages:?}");
    assert_eq!(stages.last().map(String::as_str), Some("success"));
    assert!(be.list_models().unwrap().iter().any(|m| m.name == "my-model"));
    assert_eq!(files_in(&work), 0, "the downloaded copy is cleaned up");
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn stopping_an_import_leaves_no_partial_file_or_model() {
    let server = MockServer::start();
    let be = Backend::new(&Config { base_url: server.url(), ..Config::default() });
    let url = format!("{}/files/model.gguf", server.url());
    let work = std::env::temp_dir().join(format!("cortex_import_stop{}", std::process::id()));
    let stop = CancelHandle::new();
    let err = import_from_url(&be, &url, "stopped-model", &work, &stop, &mut |p| if p.completed > 200_000 { stop.cancel() }).unwrap_err();

    assert!(err.contains("stopped"), "{err}");
    assert_eq!(files_in(&work), 0, "no half-downloaded file left behind");
    assert!(!be.list_models().unwrap().iter().any(|m| m.name == "stopped-model"));
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn stopping_during_the_upload_ends_promptly_without_a_model() {
    let server = MockServer::start();
    let be = Backend::new(&Config { base_url: server.url(), ..Config::default() });
    let url = format!("{}/files/model.gguf", server.url());
    let work = std::env::temp_dir().join(format!("cortex_import_upload{}", std::process::id()));
    let stop = CancelHandle::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let watcher = stop.clone();
    std::thread::spawn(move || {
        let r = import_from_url(&be, &url, "upload-stopped", &work, &watcher.clone(), &mut |p| if p.status.starts_with("uploading") { watcher.cancel() });
        let _ = tx.send((r, be.list_models().map(|m| m.iter().any(|m| m.name == "upload-stopped")), work));
    });
    let (result, created, work) = rx.recv_timeout(std::time::Duration::from_secs(20)).expect("a stopped upload doesn't hang");
    assert!(result.unwrap_err().contains("stopped"));
    assert!(!created.unwrap());
    assert_eq!(files_in(&work), 0);
    let _ = std::fs::remove_dir_all(&work);
    drop(stop);
}

fn files_in(dir: &std::path::Path) -> usize { std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0) }

#[test]
fn downloads_go_next_to_ollamas_models_when_ollama_is_on_this_machine() {
    use cortex_core::import::download_dir;
    let home = std::env::temp_dir().join(format!("cortex_dl_home{}", std::process::id()));
    let ollama = home.join(".ollama").join("models");
    std::fs::create_dir_all(&ollama).unwrap();
    let fallback = home.join(".cortex").join("downloads");
    let local = "http://localhost:11434";
    // Ollama's default folder, or the one OLLAMA_MODELS points to
    assert_eq!(download_dir(local, None, Some(&home), &fallback), ollama.join("cortex-import"));
    let moved = home.join("D-drive").join("models");
    std::fs::create_dir_all(&moved).unwrap();
    assert_eq!(download_dir("http://127.0.0.1:11434", Some(moved.to_str().unwrap()), Some(&home), &fallback), moved.join("cortex-import"));
    // Ollama on another machine, or no Ollama folder here: Cortex's own folder
    assert_eq!(download_dir("http://192.168.1.20:11434", None, Some(&home), &fallback), fallback);
    assert_eq!(download_dir(local, None, Some(&home.join("nobody")), &fallback), fallback);
    let _ = std::fs::remove_dir_all(&home);
}
