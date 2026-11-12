//! Which models suit a machine: a pure function of its hardware profile and the model catalog.

use crate::config::BackendKind;
use crate::hardware::HardwareProfile;
use serde::Deserialize;

/// A model in the catalog. Sizes are in GB.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CatalogEntry {
    /// The Ollama model name, e.g. "qwen2.5:7b".
    pub id: String,
    /// The name to look for in other servers, e.g. "Qwen2.5 7B Instruct".
    pub name: String,
    /// Parameters, in billions.
    pub params_b: f64,
    pub quant: String,
    pub download_gb: f64,
    /// Memory needed to run it at Cortex's default context length (weights, cache and overhead).
    pub memory_gb: f64,
    /// Whether it can call tools (Cortex's agent features need this).
    pub tools: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit { OnGpu, PartlyOnCpu, CpuOnly, DoesntFit }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed { Fast, Moderate, Slow }

impl Recommendation {
    /// What to call the model: its Ollama id where Cortex installs, else the name other servers use.
    pub fn display_name(&self, can_install: bool) -> &str {
        if can_install { &self.model.id } else { &self.model.name }
    }

    /// Ranking groups, best first.
    fn tier(&self) -> Tier {
        match self.speed { Some(Speed::Fast | Speed::Moderate) => Tier::Usable, Some(Speed::Slow) => Tier::Slow, None => Tier::TooBig }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Tier { Usable, Slow, TooBig }

#[derive(Debug, Clone, PartialEq)]
pub struct Recommendation {
    pub model: CatalogEntry,
    pub fit: Fit,
    /// None when it doesn't fit.
    pub speed: Option<Speed>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Recommendations {
    /// Tool-capable models, best first; the ones that don't fit come last.
    pub models: Vec<Recommendation>,
    /// Cortex can download these for this backend (Ollama); other servers get the advice only.
    pub can_install: bool,
}

/// The catalog bundled into this build.
pub fn catalog() -> Vec<CatalogEntry> {
    #[derive(Deserialize)]
    struct File { model: Vec<CatalogEntry> }
    toml::from_str::<File>(include_str!("../catalog.toml")).expect("the bundled catalog is valid (checked by its test)").model
}

/// GB/s a CPU reads from RAM, roughly (dual-channel DDR4/DDR5).
const CPU_BANDWIDTH: f64 = 40.0;
/// GB/s for Apple Silicon unified memory, roughly (the base chips; Pro/Max/Ultra are faster).
const UNIFIED_BANDWIDTH: f64 = 100.0;

/// Rank the catalog's tool-capable models for this hardware.
pub fn recommend(hw: &HardwareProfile, catalog: &[CatalogEntry], backend: BackendKind) -> Recommendations {
    // keep some RAM for the system and other programs
    let ram_budget = hw.total_ram_gb - (hw.total_ram_gb / 5.0).max(2.0);
    // macOS lets the GPU use about 2/3 of unified memory (3/4 on machines with more than 36 GB),
    // but never more than the system can spare
    let gpu_memory = match hw.unified_memory {
        true if hw.total_ram_gb > 36.0 => hw.total_ram_gb * 0.75,
        true => hw.total_ram_gb * 2.0 / 3.0,
        false => 0.0,
    }.min(ram_budget);
    let mut models: Vec<Recommendation> = catalog.iter().filter(|m| m.tools).map(|m| {
        let fit = if m.memory_gb <= gpu_memory { Fit::OnGpu }
            else if m.memory_gb > ram_budget { Fit::DoesntFit }
            else if gpu_memory > 0.0 { Fit::PartlyOnCpu }
            else { Fit::CpuOnly };
        let bandwidth = if fit == Fit::OnGpu { UNIFIED_BANDWIDTH } else { CPU_BANDWIDTH };
        let speed = (fit != Fit::DoesntFit).then(|| speed_tier(bandwidth / m.download_gb));
        Recommendation { model: m.clone(), fit, speed }
    }).collect();
    // usable and not slow (most capable first), then usable but slow (likewise), then too big
    // (closest to fitting first)
    models.sort_by(|a, b| a.tier().cmp(&b.tier()).then(match a.tier() {
        Tier::TooBig => a.model.memory_gb.total_cmp(&b.model.memory_gb),
        Tier::Usable | Tier::Slow => b.model.params_b.total_cmp(&a.model.params_b),
    }));
    Recommendations { models, can_install: backend == BackendKind::Ollama }
}

/// Each generated token reads all the weights once, so speed ≈ memory bandwidth / model size.
fn speed_tier(tokens_per_s: f64) -> Speed {
    if tokens_per_s >= 25.0 { Speed::Fast } else if tokens_per_s >= 7.0 { Speed::Moderate } else { Speed::Slow }
}
