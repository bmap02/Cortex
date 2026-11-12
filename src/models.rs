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
        let fit = match r.fit { Fit::OnGpu => "fits on GPU", Fit::PartlyOnCpu => "partly on CPU", Fit::CpuOnly | Fit::DoesntFit => "CPU only" };
        let speed = match r.speed { Some(Speed::Fast) => "fast", Some(Speed::Moderate) => "moderate", Some(Speed::Slow) | None => "slow" };
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
    let switch = use_it || (cfg.model != id && std::io::stdin().is_terminal()
        && crate::ask(&format!("use {id} from now on? [Y/n]"), "y").eq_ignore_ascii_case("y"));
    if switch {
        match Config::update(|c| { c.model = id.clone(); Ok(()) }) {
            Ok(()) => println!("{}", ui::muted(&format!("default model → {id}"))),
            Err(e) => { eprintln!("{}", ui::error(&e)); return 1; }
        }
    }
    0
}

/// Download `id` through Ollama unless it's there already, showing progress. Ctrl+C stops it.
pub fn download(cfg: &Config, id: &str) -> Result<(), String> {
    let be = Backend::new(cfg);
    if be.list_models().map(|m| m.iter().any(|m| m.name == id || m.name == format!("{id}:latest"))).unwrap_or(false) {
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
