//! Installing models: pulling through Ollama with progress, and keeping the catalog fresh.

use cortex_core::backend::{Backend, PullProgress};
use cortex_core::cancel::CancelHandle;
use cortex_core::config::{BackendKind, Config};
use cortex_core::recommend::{newest_catalog, parse_catalog};

mod common;
use common::MockServer;

fn backend(server: &MockServer) -> Backend {
    Backend::new(&Config { base_url: server.url(), ..Config::default() })
}

#[test]
fn pulling_reports_progress_and_the_model_is_then_installed() {
    let server = MockServer::start();
    let be = backend(&server);
    let mut seen: Vec<PullProgress> = vec![];
    be.pull("qwen2.5:3b", &CancelHandle::new(), &mut |p| seen.push(p.clone())).unwrap();

    let downloading: Vec<&PullProgress> = seen.iter().filter(|p| p.total > 0).collect();
    assert!(downloading.len() >= 2, "several progress updates: {seen:?}");
    assert!(downloading.windows(2).all(|w| w[0].completed <= w[1].completed), "progress only goes up");
    assert_eq!(downloading.last().unwrap().completed, downloading.last().unwrap().total);
    assert_eq!(seen.last().unwrap().status, "success");
    assert!(be.list_models().unwrap().iter().any(|m| m.name == "qwen2.5:3b"));
}

#[test]
fn a_pull_can_be_stopped() {
    let server = MockServer::start();
    let be = backend(&server);
    let stop = CancelHandle::new();
    let err = be.pull("qwen2.5:3b", &stop, &mut |p| if p.completed > 0 { stop.cancel() }).unwrap_err();
    assert!(err.to_string().contains("stopped"), "{err}");
}

#[test]
fn only_ollama_installs_models() {
    let server = MockServer::start();
    let be = Backend::new(&Config { backend: BackendKind::Openai, base_url: format!("{}/v1", server.url()), ..Config::default() });
    assert!(be.pull("qwen2.5:3b", &CancelHandle::new(), &mut |_| {}).is_err());
}

const OLD: &str = "format = 1\nupdated = \"2026-11-01\"\n[[model]]\nid = \"old:1b\"\nname = \"Old\"\nparams_b = 1.0\nquant = \"Q4_K_M\"\ndownload_gb = 1.0\nmemory_gb = 2.0\ntools = true\n";
const NEW: &str = "format = 1\nupdated = \"2026-12-01\"\n[[model]]\nid = \"new:1b\"\nname = \"New\"\nparams_b = 1.0\nquant = \"Q4_K_M\"\ndownload_gb = 1.0\nmemory_gb = 2.0\ntools = true\n";

#[test]
fn a_downloaded_catalog_is_used_only_when_it_is_newer() {
    let ids = |c: cortex_core::recommend::Catalog| c.models.into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(ids(newest_catalog(parse_catalog(OLD).unwrap(), Some(NEW))), ["new:1b"]);
    assert_eq!(ids(newest_catalog(parse_catalog(NEW).unwrap(), Some(OLD))), ["new:1b"], "a newer release beats an older download");
    assert_eq!(ids(newest_catalog(parse_catalog(OLD).unwrap(), Some("not a catalog"))), ["old:1b"]);
    assert_eq!(ids(newest_catalog(parse_catalog(OLD).unwrap(), None)), ["old:1b"]);
}

#[test]
fn catalogs_without_usable_models_are_rejected() {
    assert!(parse_catalog("updated = \"2026-12-01\"\nmodel = []\n").is_err(), "empty");
    assert!(parse_catalog(&NEW.replace("tools = true", "tools = false")).is_err(), "a model that can't call tools");
    assert!(parse_catalog(&NEW.replace("memory_gb = 2.0", "memory_gb = 0.5")).is_err(), "needs less memory than its download");
    assert!(parse_catalog(&NEW.replace("updated = \"2026-12-01\"\n", "")).is_err(), "no date to compare");
}

#[test]
fn a_catalog_in_a_newer_format_is_left_for_newer_cortex_versions() {
    // a later Cortex may change the catalog: this one keeps its own instead of misreading it
    let newer_format = NEW.replace("format = 1", "format = 2");
    assert!(parse_catalog(&newer_format).unwrap_err().contains("newer Cortex"));
    let with_new_field = NEW.replace("tools = true", "tools = true\nvision = true");
    assert!(parse_catalog(&with_new_field).is_err(), "a field this version doesn't know means a newer format");
    let ids = |c: cortex_core::recommend::Catalog| c.models.into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(ids(newest_catalog(parse_catalog(OLD).unwrap(), Some(&newer_format))), ["old:1b"]);
}
