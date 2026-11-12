//! The recommender as a pure function: fixed hardware profiles and a fixed catalog in, ranked
//! recommendations with fit verdicts and speed tiers out.

use cortex_core::config::BackendKind;
use cortex_core::hardware::HardwareProfile;
use cortex_core::recommend::{recommend, CatalogEntry, Fit, Speed};

fn entry(id: &str, params_b: f64, download_gb: f64, memory_gb: f64, tools: bool) -> CatalogEntry {
    CatalogEntry { id: id.into(), name: id.into(), params_b, quant: "Q4_K_M".into(), download_gb, memory_gb, tools }
}

/// Sizes in the range of real 1B, 3B, 7B, 14B and 32B models at 4-bit.
fn catalog() -> Vec<CatalogEntry> {
    vec![
        entry("tiny", 1.0, 1.3, 2.0, true),
        entry("small", 3.0, 2.0, 3.0, true),
        entry("mid", 7.0, 4.7, 6.0, true),
        entry("chat-only", 7.0, 4.7, 6.0, false),
        entry("large", 14.0, 9.0, 11.0, true),
        entry("huge", 32.0, 20.0, 22.0, true),
    ]
}

fn ram_only(total_gb: f64) -> HardwareProfile {
    HardwareProfile { total_ram_gb: total_gb, available_ram_gb: total_gb / 2.0, unified_memory: false, os: "linux".into(), arch: "x86_64".into() }
}

/// (id, fit, speed) in ranked order.
fn ranked(hw: &HardwareProfile) -> Vec<(String, Fit, Option<Speed>)> {
    recommend(hw, &catalog(), BackendKind::Ollama).models.into_iter().map(|r| (r.model.id, r.fit, r.speed)).collect()
}

#[test]
fn cpu_only_8gb_gets_small_models_on_the_cpu() {
    // 8 GB, 2 GB kept for the system: up to 6 GB for the model; CPU memory ~40 GB/s
    assert_eq!(ranked(&ram_only(8.0)), [
        ("mid".into(), Fit::CpuOnly, Some(Speed::Moderate)),   // 40 / 4.7 GB ≈ 8.5 tok/s
        ("small".into(), Fit::CpuOnly, Some(Speed::Moderate)), // ≈ 20 tok/s
        ("tiny".into(), Fit::CpuOnly, Some(Speed::Fast)),      // ≈ 31 tok/s
        ("large".into(), Fit::DoesntFit, None),
        ("huge".into(), Fit::DoesntFit, None),
    ]);
}

#[test]
fn apple_unified_16gb_runs_models_on_the_gpu() {
    let hw = HardwareProfile { total_ram_gb: 16.0, available_ram_gb: 8.0, unified_memory: true, os: "macos".into(), arch: "aarch64".into() };
    // the GPU may use about 2/3 of unified memory (≈ 10.7 GB) at ~100 GB/s; 12.8 GB in all for a model
    assert_eq!(ranked(&hw), [
        ("mid".into(), Fit::OnGpu, Some(Speed::Moderate)),     // 100 / 4.7 ≈ 21 tok/s
        ("small".into(), Fit::OnGpu, Some(Speed::Fast)),       // 50 tok/s
        ("tiny".into(), Fit::OnGpu, Some(Speed::Fast)),
        ("large".into(), Fit::PartlyOnCpu, Some(Speed::Slow)), // 11 GB: more than the GPU's share; CPU speed ≈ 4.4
        ("huge".into(), Fit::DoesntFit, None),
    ]);
}

#[test]
fn ram_32gb_fits_big_models_but_ranks_slow_ones_after_usable_ones() {
    // 25.6 GB for a model: everything fits on the CPU, but 14B and 32B run under 8 tok/s
    assert_eq!(ranked(&ram_only(32.0)), [
        ("mid".into(), Fit::CpuOnly, Some(Speed::Moderate)),
        ("small".into(), Fit::CpuOnly, Some(Speed::Moderate)),
        ("tiny".into(), Fit::CpuOnly, Some(Speed::Fast)),
        ("huge".into(), Fit::CpuOnly, Some(Speed::Slow)),  // 40 / 20 = 2 tok/s
        ("large".into(), Fit::CpuOnly, Some(Speed::Slow)), // 40 / 9 ≈ 4.4 tok/s
    ]);
}

#[test]
fn only_ollama_can_install_the_recommendations() {
    assert!(recommend(&ram_only(16.0), &catalog(), BackendKind::Ollama).can_install);
    let other = recommend(&ram_only(16.0), &catalog(), BackendKind::Openai);
    assert!(!other.can_install);
    assert_eq!(other.models, recommend(&ram_only(16.0), &catalog(), BackendKind::Ollama).models, "same advice either way");
}

#[test]
fn the_bundled_catalog_is_usable() {
    let catalog = cortex_core::recommend::catalog();
    assert!(catalog.len() >= 8);
    for m in &catalog {
        assert!(m.tools, "{} is in the catalog, so it should call tools", m.id);
        assert!(m.memory_gb > m.download_gb && m.download_gb > 0.0 && m.params_b > 0.0, "{}: sizes look wrong", m.id);
    }
    // every machine with 8 GB of RAM gets at least one model that fits
    assert!(recommend(&ram_only(8.0), &catalog, BackendKind::Ollama).models.iter().any(|r| r.fit != Fit::DoesntFit));
}

#[test]
fn a_small_mac_never_gets_more_on_the_gpu_than_it_can_spare() {
    // 4 GB unified: 2/3 would be 2.7 GB for the GPU, but only 2 GB is left after the system's share
    let hw = HardwareProfile { total_ram_gb: 4.0, available_ram_gb: 2.0, unified_memory: true, os: "macos".into(), arch: "aarch64".into() };
    let between = entry("between", 2.0, 1.8, 2.5, true); // within the GPU's 2/3, over what the machine can spare
    let r = recommend(&hw, &[between], BackendKind::Ollama);
    assert_eq!(r.models[0].fit, Fit::DoesntFit);
}
