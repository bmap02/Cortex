//! The recommender as a pure function: fixed hardware profiles and a fixed catalog in, ranked
//! recommendations with fit verdicts and speed tiers out.

use cortex_core::config::BackendKind;
use cortex_core::hardware::{Gpu, GpuVendor, HardwareProfile};
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
    HardwareProfile { total_ram_gb: total_gb, available_ram_gb: total_gb / 2.0, unified_memory: false, gpus: Some(vec![]), os: "linux".into(), arch: "x86_64".into() }
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
    let hw = HardwareProfile { total_ram_gb: 16.0, available_ram_gb: 8.0, unified_memory: true, gpus: Some(vec![]), os: "macos".into(), arch: "aarch64".into() };
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
    let hw = HardwareProfile { total_ram_gb: 4.0, available_ram_gb: 2.0, unified_memory: true, gpus: Some(vec![]), os: "macos".into(), arch: "aarch64".into() };
    let between = entry("between", 2.0, 1.8, 2.5, true); // within the GPU's 2/3, over what the machine can spare
    let r = recommend(&hw, &[between], BackendKind::Ollama);
    assert_eq!(r.models[0].fit, Fit::DoesntFit);
}

fn gpu(vendor: GpuVendor, vram_gb: f64) -> Gpu {
    Gpu { vendor, name: format!("{vendor:?} {vram_gb} GB"), vram_gb, integrated: false }
}

fn with_gpus(ram_gb: f64, gpus: Vec<Gpu>) -> HardwareProfile {
    HardwareProfile { gpus: Some(gpus), ..ram_only(ram_gb) }
}

#[test]
fn an_8gb_nvidia_card_runs_mid_models_and_offloads_bigger_ones() {
    // 7.2 GB of the card for models (10% kept back); at ~300 GB/s VRAM vs ~40 GB/s RAM
    assert_eq!(ranked(&with_gpus(16.0, vec![gpu(GpuVendor::Nvidia, 8.0)])), [
        ("large".into(), Fit::PartlyOnCpu, Some(Speed::Moderate)), // 7.2 of 11 GB on the GPU ≈ 10 tok/s
        ("mid".into(), Fit::OnGpu, Some(Speed::Fast)),            // 300 / 4.7 ≈ 64 tok/s
        ("small".into(), Fit::OnGpu, Some(Speed::Fast)),
        ("tiny".into(), Fit::OnGpu, Some(Speed::Fast)),
        ("huge".into(), Fit::DoesntFit, None),                    // 14.8 GB left over; RAM can spare 12.8
    ]);
}

#[test]
fn a_24gb_card_offloads_only_a_little_of_a_32b_model() {
    assert_eq!(ranked(&with_gpus(32.0, vec![gpu(GpuVendor::Nvidia, 24.0)])), [
        ("huge".into(), Fit::PartlyOnCpu, Some(Speed::Moderate)), // 21.6 of 22 GB on the GPU ≈ 13 tok/s
        ("large".into(), Fit::OnGpu, Some(Speed::Fast)),          // 300 / 9 ≈ 33 tok/s
        ("mid".into(), Fit::OnGpu, Some(Speed::Fast)),
        ("small".into(), Fit::OnGpu, Some(Speed::Fast)),
        ("tiny".into(), Fit::OnGpu, Some(Speed::Fast)),
    ]);
}

#[test]
fn an_integrated_gpu_counts_as_cpu_only() {
    let igpu = Gpu { vendor: GpuVendor::Intel, name: "Intel UHD Graphics".into(), vram_gb: 0.1, integrated: true };
    assert_eq!(ranked(&with_gpus(16.0, vec![igpu])), ranked(&ram_only(16.0)));
}

#[test]
fn gpus_of_one_vendor_share_a_model_and_other_vendors_do_not_add() {
    let two_12gb_and_an_amd = vec![gpu(GpuVendor::Nvidia, 12.0), gpu(GpuVendor::Nvidia, 12.0), gpu(GpuVendor::Amd, 8.0)];
    assert_eq!(ranked(&with_gpus(32.0, two_12gb_and_an_amd)), ranked(&with_gpus(32.0, vec![gpu(GpuVendor::Nvidia, 24.0)])));
    // the bigger vendor group wins even when it isn't listed first
    let amd_first = vec![gpu(GpuVendor::Amd, 8.0), gpu(GpuVendor::Nvidia, 24.0)];
    assert_eq!(ranked(&with_gpus(32.0, amd_first)), ranked(&with_gpus(32.0, vec![gpu(GpuVendor::Nvidia, 24.0)])));
}

#[test]
fn when_gpu_detection_fails_recommendations_use_ram_only() {
    let unknown = HardwareProfile { gpus: None, ..ram_only(16.0) };
    assert_eq!(ranked(&unknown), ranked(&ram_only(16.0)));
}

#[test]
fn ollama_only_counts_gpus_it_can_run_models_on() {
    // Ollama runs on NVIDIA and AMD; llama.cpp-based servers (Vulkan) use Intel Arc too
    let arc = with_gpus(16.0, vec![gpu(GpuVendor::Intel, 16.0)]);
    let on_ollama = recommend(&arc, &catalog(), BackendKind::Ollama).models;
    assert!(on_ollama.iter().all(|r| r.fit != Fit::OnGpu), "Ollama can't use the Arc card");
    assert_eq!(on_ollama, recommend(&ram_only(16.0), &catalog(), BackendKind::Ollama).models);
    let elsewhere = recommend(&arc, &catalog(), BackendKind::Openai).models;
    assert_eq!(elsewhere[0].fit, Fit::OnGpu);
}
