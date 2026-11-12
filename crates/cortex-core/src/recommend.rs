//! Which models suit a machine: `recommend` is a pure function of a hardware profile and a
//! catalog. The catalog is bundled into the binary, and `cortex update` can save a newer one in
//! the Cortex home folder (`catalog`, `save_downloaded_catalog`).

use crate::config::{self, BackendKind};
use crate::hardware::{GpuVendor, HardwareProfile};
use serde::Deserialize;

/// A model in the catalog. Sizes are in GB.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)] // a field this version doesn't know means a format it can't read
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

/// A model catalog and the date it was last changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    /// YYYY-MM-DD, so later dates compare as greater.
    pub updated: String,
    pub models: Vec<CatalogEntry>,
}

/// The catalog layout this version reads. Bump it (and the `format` in catalog.toml) when a change
/// would make older versions misread the file, such as a new field; they then keep their own.
pub const CATALOG_FORMAT: u32 = 1;

/// Read a catalog file, refusing one Cortex couldn't recommend from.
pub fn parse_catalog(text: &str) -> Result<Catalog, String> {
    #[derive(Deserialize)]
    struct Format { format: Option<u32> }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File { #[allow(dead_code)] format: u32, updated: String, model: Vec<CatalogEntry> }
    let format = toml::from_str::<Format>(text).ok().and_then(|f| f.format);
    if format.is_some_and(|f| f > CATALOG_FORMAT) {
        return Err("the catalog is for a newer Cortex".into());
    }
    let f: File = toml::from_str(text).map_err(|e| format!("not a model catalog: {e}"))?;
    let is_date = f.updated.len() == 10 && f.updated.chars().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() });
    if !is_date { return Err(format!("the catalog's date '{}' isn't YYYY-MM-DD", f.updated)); }
    if f.model.is_empty() { return Err("the catalog lists no models".into()); }
    for m in &f.model {
        if !m.tools { return Err(format!("{} can't call tools, so it doesn't belong in the catalog", m.id)); }
        if !(m.params_b > 0.0 && m.download_gb > 0.0 && m.memory_gb > m.download_gb) {
            return Err(format!("{}: its sizes don't add up (it needs more memory than its download)", m.id));
        }
    }
    Ok(Catalog { updated: f.updated, models: f.model })
}

/// The catalog bundled into this build.
pub fn bundled_catalog() -> Catalog {
    parse_catalog(include_str!("../catalog.toml")).expect("the bundled catalog is valid (checked by its test)")
}

/// The bundled catalog, or a downloaded one if it's valid and newer.
pub fn newest_catalog(bundled: Catalog, downloaded: Option<&str>) -> Catalog {
    match downloaded.and_then(|t| parse_catalog(t).ok()) {
        Some(d) if d.updated > bundled.updated => d,
        _ => bundled,
    }
}

/// The catalog in use: the newest of the bundled one and the one `cortex update` last saved.
fn current_catalog() -> Catalog {
    newest_catalog(bundled_catalog(), std::fs::read_to_string(config::catalog_path()).ok().as_deref())
}

/// The models to recommend from.
pub fn catalog() -> Vec<CatalogEntry> { current_catalog().models }

/// Keep a downloaded catalog for later if it's valid and newer than the one in use. Returns its
/// date when it was kept, None when the one in use is as new (or the download is for a newer
/// Cortex, which this one can't read).
pub fn save_downloaded_catalog(text: &str) -> Result<Option<String>, String> {
    let new = match parse_catalog(text) {
        Err(e) if e.contains("newer Cortex") => return Ok(None),
        other => other?,
    };
    if new.updated <= current_catalog().updated { return Ok(None); }
    let _ = std::fs::create_dir_all(config::home());
    std::fs::write(config::catalog_path(), text).map_err(|e| format!("couldn't save the catalog: {e}"))?;
    Ok(Some(new.updated))
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

/// Memory a model file needs to run at the default context: the weights, plus the context cache
/// and runtime, which grow with the model (this matches the catalog's own figures closely).
pub fn estimate_memory_gb(download_gb: f64) -> f64 { download_gb * 1.1 + 0.5 }

/// The fit check for a model that isn't in the catalog, from the size of its file: the same
/// verdict and speed tier a catalog model of that size would get.
pub fn check_fit(hw: &HardwareProfile, download_gb: f64, backend: BackendKind) -> Recommendation {
    let model = CatalogEntry {
        id: String::new(), name: String::new(), params_b: 0.0, quant: String::new(),
        download_gb, memory_gb: estimate_memory_gb(download_gb), tools: true,
    };
    recommend(hw, &[model], backend).models.remove(0)
}
