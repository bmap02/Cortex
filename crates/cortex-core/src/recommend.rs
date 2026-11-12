//! Which models suit a machine: a pure function of its hardware profile and the model catalog.

use crate::config::BackendKind;
use crate::hardware::{GpuVendor, HardwareProfile};
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
/// GB/s for a dedicated GPU's memory, roughly (mid-range cards; the fastest reach 1000).
const VRAM_BANDWIDTH: f64 = 300.0;

/// What the GPU can hold of a model, and how fast it reads it.
struct GpuShare { memory_gb: f64, bandwidth: f64 }

fn gpu_share(hw: &HardwareProfile, ram_budget: f64, backend: BackendKind) -> GpuShare {
    if hw.unified_memory {
        // macOS lets the GPU use about 2/3 of unified memory (3/4 on machines with more than
        // 36 GB), but never more than the system can spare
        let share = if hw.total_ram_gb > 36.0 { 0.75 } else { 2.0 / 3.0 };
        return GpuShare { memory_gb: (hw.total_ram_gb * share).min(ram_budget), bandwidth: UNIFIED_BANDWIDTH };
    }
    // Ollama runs models on NVIDIA (CUDA) and AMD (ROCm, not on macOS); llama.cpp-based servers
    // can use any GPU through Vulkan
    let usable_by_backend = |v: GpuVendor| backend != BackendKind::Ollama
        || v == GpuVendor::Nvidia || (v == GpuVendor::Amd && hw.os != "macos");
    // a model is split across GPUs of one vendor, not across vendors: take the vendor with the
    // most memory. Keep 10% of each card (at least 0.5 GB) for the display and the runtime.
    let mut by_vendor: Vec<(GpuVendor, f64)> = vec![];
    for g in hw.gpus.iter().flatten().filter(|g| !g.integrated && g.vram_gb > 0.0 && usable_by_backend(g.vendor)) {
        let usable = (g.vram_gb - (g.vram_gb / 10.0).max(0.5)).max(0.0);
        match by_vendor.iter_mut().find(|(v, _)| *v == g.vendor) {
            Some((_, total)) => *total += usable,
            None => by_vendor.push((g.vendor, usable)),
        }
    }
    GpuShare { memory_gb: by_vendor.into_iter().map(|(_, gb)| gb).fold(0.0, f64::max), bandwidth: VRAM_BANDWIDTH }
}

/// Rank the catalog's tool-capable models for this hardware.
pub fn recommend(hw: &HardwareProfile, catalog: &[CatalogEntry], backend: BackendKind) -> Recommendations {
    // keep some RAM for the system and other programs
    let ram_budget = hw.total_ram_gb - (hw.total_ram_gb / 5.0).max(2.0);
    let GpuShare { memory_gb: gpu_memory, bandwidth: gpu_bandwidth } = gpu_share(hw, ram_budget, backend);
    let mut models: Vec<Recommendation> = catalog.iter().filter(|m| m.tools).map(|m| {
        // what doesn't fit on the GPU goes to RAM; unified memory is the same RAM, so it can't
        // spill into more than the budget there
        let spill = if hw.unified_memory { m.memory_gb } else { m.memory_gb - gpu_memory };
        let fit = if m.memory_gb <= gpu_memory { Fit::OnGpu }
            else if spill > ram_budget { Fit::DoesntFit }
            else if gpu_memory > 0.0 { Fit::PartlyOnCpu }
            else { Fit::CpuOnly };
        let speed = match fit {
            Fit::OnGpu => Some(gpu_bandwidth / m.download_gb),
            Fit::CpuOnly => Some(CPU_BANDWIDTH / m.download_gb),
            // the GPU reads its share of the weights, the CPU the rest, one after the other
            Fit::PartlyOnCpu if hw.unified_memory => Some(CPU_BANDWIDTH / m.download_gb),
            Fit::PartlyOnCpu => {
                let on_gpu = gpu_memory / m.memory_gb;
                Some(1.0 / (m.download_gb * (on_gpu / gpu_bandwidth + (1.0 - on_gpu) / CPU_BANDWIDTH)))
            }
            Fit::DoesntFit => None,
        }.map(speed_tier);
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
