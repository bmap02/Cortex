//! `cortex models recommend` and `cortex models install`, and the model step of `cortex setup`.

use crate::ui;
use cortex_core::backend::{Backend, PullProgress};
use cortex_core::config::{BackendKind, Config};
use cortex_core::hardware::HardwareProfile;
use cortex_core::recommend::{self, Fit, Recommendation, Recommendations, Speed};
use std::io::{IsTerminal, Write};

/// This machine's recommendations, or None (with the reason printed) when memory can't be read.
fn recommendations(cfg: &Config) -> Option<(HardwareProfile, Recommendations)> {
    let hw = cortex_core::hardware::probe();
    if hw.total_ram_gb <= 0.0 {
        eprintln!("{}", ui::error("Couldn't read how much memory this machine has, so there's nothing to recommend from."));
        return None;
    }
    let recs = recommend::recommend(&hw, &recommend::catalog(), cfg.backend);
    Some((hw, recs))
}

/// The recommendations that fit, best first: the ones the list numbers.
fn fitting(recs: &Recommendations) -> Vec<&Recommendation> {
    recs.models.iter().filter(|r| r.fit != Fit::DoesntFit).collect()
}

/// What `cortex models recommend` prints.
pub enum Show { Table, Best, Names }

/// `cortex models recommend`: the table, or for scripts just the best model's name or every
/// fitting model's name, one per line, in the table's order.
pub fn print_recommendations(cfg: &Config, show: Show) -> i32 {
    let Some((hw, recs)) = recommendations(cfg) else { return 1 };
    let names: Vec<&str> = fitting(&recs).into_iter().map(|r| r.display_name(recs.can_install)).collect();
    match show {
        Show::Best => return match names.first() { Some(n) => { println!("{n}"); 0 } None => 1 },
        Show::Names => { for n in names { println!("{n}"); } return 0; }
        Show::Table => {}
    }
    let (summary, details) = describe_hardware(&hw);
    println!("{}", ui::bold(&format!("This machine: {summary}")));
    for d in details { println!("{}", ui::faint(&format!("  {d}"))); }
    println!();
    print_list(cfg, &recs, usize::MAX, true);
    println!();
    if recs.can_install {
        println!("{}", ui::faint("  Install one with `cortex models install <number or name>` (add --use to switch to it)."));
    } else {
        println!("{}", ui::faint("  Recommendations only: Cortex installs models through Ollama, and you're using an OpenAI-compatible server."));
        println!("{}", ui::faint("  Load one of these in your server, then pick it with `cortex model <name>`."));
    }
    0
}

/// The numbered list of models that fit (at most `limit`), then optionally the ones that don't.
fn print_list(cfg: &Config, recs: &Recommendations, limit: usize, show_too_big: bool) {
    let fits = fitting(recs);
    if fits.is_empty() { println!("{}", ui::error("None of the models Cortex knows fit in this machine's memory.")); }
    for (i, r) in fits.iter().enumerate().take(limit) {
        let m = &r.model;
        // other servers name models differently: show what to look for there
        let shown = r.display_name(recs.can_install);
        let (fit, speed) = (fit_label(r.fit), speed_label(r.speed));
        let params = if m.params_b < 10.0 { format!("{:.1}B", m.params_b) } else { format!("{:.0}B", m.params_b) };
        // pad before styling: escape codes would count toward the width
        let mark = if m.id == cfg.model || m.name == cfg.model { ui::bold(&format!("{:>3}", ui::DOT)) } else { ui::faint(&format!("{:>2}.", i + 1)) };
        println!("  {mark} {shown:<30} {}  {}  {fit:<14} {}", ui::faint(&format!("{params:>5}")), ui::faint(&format!("{:>4.1} GB", m.download_gb)), ui::muted(speed));
    }
    let too_big: Vec<&str> = recs.models.iter().filter(|r| r.fit == Fit::DoesntFit).map(|r| r.display_name(recs.can_install)).collect();
    if !too_big.is_empty() && show_too_big {
        println!("  {}", ui::faint(&format!("too big for this machine: {}", too_big.join(", "))));
    }
}

fn fit_label(fit: Fit) -> &'static str {
    match fit { Fit::OnGpu => "fits on GPU", Fit::PartlyOnCpu => "partly on CPU", Fit::CpuOnly => "CPU only", Fit::DoesntFit => "doesn't fit" }
}

fn speed_label(speed: Option<Speed>) -> &'static str {
    match speed { Some(Speed::Fast) => "fast", Some(Speed::Moderate) => "moderate", Some(Speed::Slow) => "slow", None => "" }
}

/// A one-line summary of the hardware, and a line per GPU (or why there are none).
pub fn describe_hardware(hw: &HardwareProfile) -> (String, Vec<String>) {
    let memory = if hw.unified_memory { "unified memory, shared with the GPU" } else { "RAM" };
    let summary = format!("{:.0} GB {memory} ({:.1} GB free) · {} {}", hw.total_ram_gb, hw.available_ram_gb, hw.os, hw.arch);
    let details = match &hw.gpus {
        _ if hw.unified_memory => vec![],
        None => vec!["Couldn't detect GPUs: models are rated for the CPU only.".to_string()],
        Some(gpus) if gpus.is_empty() => vec!["No GPU found: models are rated for the CPU.".to_string()],
        Some(gpus) => gpus.iter().map(|g| match (g.integrated, g.vram_gb > 0.0) {
            (true, _) => format!("GPU: {} (built in, shares RAM: not used for models)", g.name),
            (false, true) => format!("GPU: {}, {:.1} GB", g.name, g.vram_gb),
            (false, false) => format!("GPU: {} (couldn't read its memory: not used for models)", g.name),
        }).collect(),
    };
    (summary, details)
}

/// The catalog id for what the user typed: a number from the recommendation list, or an id.
fn resolve(target: &str, recs: &Recommendations) -> Result<String, String> {
    let target = target.trim();
    if let Ok(n) = target.parse::<usize>() {
        let fits = fitting(recs);
        return fits.get(n.wrapping_sub(1)).map(|r| r.model.id.clone())
            .ok_or_else(|| format!("there's no number {n} in the list (1 to {}); see `cortex models recommend`", fits.len()));
    }
    match recs.models.iter().find(|r| r.model.id == target) {
        Some(r) if r.fit == Fit::DoesntFit => Err(format!("{target} is too big for this machine; see `cortex models recommend`")),
        Some(r) => Ok(r.model.id.clone()),
        None => Err(format!("{target} isn't in Cortex's catalog; to download it anyway, run `cortex models pull {target}`")),
    }
}

/// `cortex models install`: download a recommended model and optionally switch to it.
pub fn install(cfg: &Config, target: Option<String>, use_it: bool) -> i32 {
    if cfg.backend != BackendKind::Ollama {
        eprintln!("{}", ui::error("Cortex installs models through Ollama; you're using an OpenAI-compatible server."));
        eprintln!("{}", ui::faint("  See `cortex models recommend` for what to load in your server."));
        return 1;
    }
    let Some((_, recs)) = recommendations(cfg) else { return 1 };
    let target = match target {
        Some(t) => t,
        None if std::io::stdin().is_terminal() => {
            print_list(cfg, &recs, 8, false);
            crate::ask("install which?", "1")
        }
        None => { eprintln!("usage: cortex models install <number or name> [--use]"); return 2; }
    };
    let id = match resolve(&target, &recs) {
        Ok(id) => id,
        Err(e) => { eprintln!("{}", ui::error(&e)); return 1; }
    };
    if let Err(e) = download(cfg, &id) { eprintln!("{}", ui::error(&e)); return 1; }
    offer_switch(cfg, &id, use_it)
}

/// Make `id` the default model: with --use, or if the user says so at a terminal.
fn offer_switch(cfg: &Config, id: &str, use_it: bool) -> i32 {
    let switch = use_it || (cfg.model != id && std::io::stdin().is_terminal()
        // the [Y/n] says what Enter means; ask's own default hint would show it twice
        && matches!(crate::ask(&format!("use {id} from now on? [Y/n]"), "").to_lowercase().as_str(), "" | "y" | "yes"));
    if !switch { return 0; }
    match Config::update(|c| { c.model = id.to_string(); Ok(()) }) {
        Ok(()) => { println!("{}", ui::muted(&format!("default model → {id}"))); 0 }
        Err(e) => { eprintln!("{}", ui::error(&e)); 1 }
    }
}

/// `cortex models import`: a GGUF from Hugging Face or a URL, after checking it will fit.
pub fn import(cfg: &Config, source: &str, quant: Option<&str>, name: Option<String>, use_it: bool, yes: bool) -> i32 {
    use cortex_core::import::{self, Source};
    if cfg.backend != BackendKind::Ollama {
        eprintln!("{}", ui::error("Cortex imports models through Ollama; you're using an OpenAI-compatible server."));
        eprintln!("{}", ui::faint("  Download the GGUF yourself and load it in your server."));
        return 1;
    }
    let src = match import::parse_source(source, quant) {
        Ok(s) => s,
        Err(e) => { eprintln!("{}", ui::error(&e)); return 1; }
    };
    let id = match (&src, name) {
        (Source::Url(_), Some(n)) => n,
        (Source::HuggingFace { .. }, Some(_)) => {
            println!("{}", ui::faint("  (--name only applies to URL imports; Ollama names Hugging Face models hf.co/...)"));
            src.ollama_name()
        }
        (_, None) => src.ollama_name(),
    };
    // URL imports name the model themselves (Hugging Face names are hf.co/owner/repo:QUANT)
    if matches!(src, Source::Url(_)) && !import::valid_model_name(&id) {
        eprintln!("{}", ui::error(&format!("'{id}' can't be a model name: use letters, digits, . _ and - (optionally namespace/name:tag)")));
        return 1;
    }
    if matches!(src, Source::Url(_)) && Backend::new(cfg).has_model(&id) {
        eprintln!("{}", ui::error(&format!("there's already a model called {id}; choose another name with --name")));
        return 1;
    }
    // the size, for the fit check before anything is downloaded: from Hugging Face, the server's
    // headers, or else guessed from the file name's parameter count and quantization
    let size = match &src {
        Source::HuggingFace { repo, quant } => match import::hf_gguf_size(import::HF_API, repo, quant) {
            Ok(b) => Some((b as f64 / 1e9, false)),
            Err(e) => { eprintln!("{}", ui::error(&e)); return 1; }
        },
        Source::Url(url) => import::url_size(url).map(|b| (b as f64 / 1e9, false))
            .or_else(|| import::estimate_size_gb(url.rsplit('/').next().unwrap_or(url)).map(|gb| (gb, true))),
    };
    if !confirm_fit(cfg, size, yes) { return 1; }
    let result = match &src {
        Source::HuggingFace { .. } => download(cfg, &id),
        Source::Url(url) => import_url(cfg, url, &id),
    };
    if let Err(e) = result { eprintln!("{}", ui::error(&e)); return 1; }
    offer_switch(cfg, &id, use_it)
}

/// Show the fit verdict for a file of `size` GB (and whether that was guessed from its name), and
/// ask before going on when it won't fit. False when the user (or a script without --yes) stops.
fn confirm_fit(cfg: &Config, size: Option<(f64, bool)>, yes: bool) -> bool {
    let Some((gb, guessed)) = size else {
        println!("{}", ui::faint("  Neither the server nor the file name says how big it is, so Cortex can't check that it fits."));
        return true;
    };
    if guessed { println!("{}", ui::faint("  (size guessed from the file name: the server didn't say)")); }
    let hw = cortex_core::hardware::probe();
    let r = recommend::check_fit(&hw, gb, cfg.backend);
    let speed = speed_label(r.speed);
    let verdict = if speed.is_empty() { fit_label(r.fit).to_string() } else { format!("{}, {speed}", fit_label(r.fit)) };
    println!("{}", ui::muted(&format!("  {gb:.1} GB file, about {:.1} GB to run: {verdict} on this machine", recommend::estimate_memory_gb(gb))));
    if r.fit != Fit::DoesntFit || yes { return true; }
    if !std::io::stdin().is_terminal() {
        eprintln!("{}", ui::error("It probably won't fit in this machine's memory. Add --yes to download it anyway."));
        return false;
    }
    crate::ask("It probably won't fit in this machine's memory. Download it anyway? [y/N]", "").eq_ignore_ascii_case("y")
}

/// Download a GGUF from `url` and create it in Ollama as `id`, with progress. Ctrl+C stops it.
fn import_url(cfg: &Config, url: &str, id: &str) -> Result<(), String> {
    let be = Backend::new(cfg);
    println!("{}", ui::bold(&format!("Importing {id}")));
    crate::CTRL_C.reset();
    let mut progress = Progress::default();
    let fallback = cortex_core::config::home().join("downloads");
    let ollama_models = std::env::var("OLLAMA_MODELS").ok();
    let mut work = cortex_core::import::download_dir(&cfg.base_url, ollama_models.as_deref(), cortex_core::config::user_home().as_deref(), &fallback);
    // e.g. Linux's system Ollama keeps models in a folder only it can write to
    if std::fs::create_dir_all(&work).is_err() { work = fallback; }
    let result = cortex_core::import::import_from_url(&be, url, id, &work, &crate::CTRL_C, &mut |p| progress.show(p));
    let _ = std::fs::remove_dir(&work); // only if it's empty: never anything of Ollama's
    progress.end_line();
    result.map_err(|e| format!("couldn't import {id}: {e}"))?;
    println!("{}", ui::muted(&format!("{id} is ready")));
    Ok(())
}

/// Download `id` through Ollama unless it's there already, showing progress. Ctrl+C stops it.
pub fn download(cfg: &Config, id: &str) -> Result<(), String> {
    let be = Backend::new(cfg);
    if be.has_model(id) {
        println!("{}", ui::muted(&format!("{id} is already downloaded")));
        return Ok(());
    }
    println!("{}", ui::bold(&format!("Downloading {id}")));
    crate::CTRL_C.reset();
    let mut progress = Progress::default();
    let result = be.pull(id, &crate::CTRL_C, &mut |p| progress.show(p));
    progress.end_line();
    result.map_err(|e| format!("couldn't download {id}: {e}"))?;
    println!("{}", ui::muted(&format!("{id} is ready")));
    Ok(())
}

/// Download progress on stderr: one line that updates in place while a part downloads, and a
/// line for each other step.
#[derive(Default)]
struct Progress {
    last: String,
    /// Width of the line being updated in place (0 when there isn't one).
    inline: usize,
}

impl Progress {
    fn show(&mut self, p: &PullProgress) {
        if p.total > 0 {
            let (c, t) = (p.completed as f64, p.total as f64);
            let line = format!("  {}  {:>5.1}%  ({:.2}/{:.2} GB)", p.status, c / t * 100.0, c / 1e9, t / 1e9);
            // pad over whatever was there before
            eprint!("\r{line:<width$}", width = self.inline.max(line.chars().count()));
            self.inline = line.chars().count();
        } else if p.status != self.last {
            self.end_line();
            eprintln!("  {}", p.status);
        }
        let _ = std::io::stderr().flush();
        self.last.clone_from(&p.status);
    }

    fn end_line(&mut self) {
        if self.inline > 0 { eprintln!(); self.inline = 0; }
    }
}

/// `cortex setup` on Ollama with no models yet: offer the recommendations and install the
/// chosen one. Returns its id, or None when the user skips.
pub fn choose_for_setup(cfg: &Config) -> Option<String> {
    let (_, recs) = recommendations(cfg)?;
    if fitting(&recs).is_empty() {
        eprintln!("{}", ui::error("None of the models Cortex knows fit in this machine's memory; skipping the download."));
        return None;
    }
    println!("{}", ui::bold("Models that suit this machine (best first):"));
    print_list(cfg, &recs, 6, false);
    loop {
        let pick = crate::ask("install which? (number or name, or 'skip')", "1");
        if pick.eq_ignore_ascii_case("skip") || pick == "-" { return None; }
        match resolve(&pick, &recs).and_then(|id| download(cfg, &id).map(|()| id)) {
            Ok(id) => return Some(id),
            Err(e) => eprintln!("{}", ui::error(&e)),
        }
    }
}

/// The names of the best few recommendations, for a one-line hint (empty if none fit).
pub fn top_names(cfg: &Config, n: usize) -> Vec<String> {
    let hw = cortex_core::hardware::probe();
    if hw.total_ram_gb <= 0.0 { return vec![]; }
    let recs = recommend::recommend(&hw, &recommend::catalog(), cfg.backend);
    fitting(&recs).into_iter().take(n).map(|r| r.display_name(recs.can_install).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex_core::recommend::CatalogEntry;

    fn rec(id: &str, fit: Fit) -> Recommendation {
        let model = CatalogEntry { id: id.into(), name: id.into(), params_b: 7.0, quant: "Q4_K_M".into(), download_gb: 4.7, memory_gb: 6.0, tools: true };
        Recommendation { model, fit, speed: (fit != Fit::DoesntFit).then_some(Speed::Fast) }
    }

    #[test]
    fn install_takes_a_list_number_or_a_catalog_id() {
        let recs = Recommendations { models: vec![rec("a:7b", Fit::OnGpu), rec("b:3b", Fit::CpuOnly), rec("huge:70b", Fit::DoesntFit)], can_install: true };
        assert_eq!(resolve("1", &recs).unwrap(), "a:7b");
        assert_eq!(resolve(" 2 ", &recs).unwrap(), "b:3b");
        assert!(resolve("3", &recs).is_err(), "the list only numbers models that fit");
        assert!(resolve("0", &recs).is_err());
        assert_eq!(resolve("b:3b", &recs).unwrap(), "b:3b");
        assert!(resolve("huge:70b", &recs).unwrap_err().contains("too big"));
        assert!(resolve("other:1b", &recs).unwrap_err().contains("cortex models pull"));
    }
}
