//! What this machine has to run a model with.

use std::sync::LazyLock;

/// Bytes to GB (10^9, like model downloads) and MiB to GB.
fn gb(bytes: u64) -> f64 { bytes as f64 / 1e9 }
fn mib_to_gb(mib: f64) -> f64 { mib * 1_048_576.0 / 1e9 }

/// The hardware a recommendation is made for. Sizes are in GB (10^9 bytes, like model downloads).
#[derive(Debug, Clone, PartialEq)]
pub struct HardwareProfile {
    pub total_ram_gb: f64,
    /// RAM free right now (other programs come and go, so this is only a hint).
    pub available_ram_gb: f64,
    /// RAM the GPU shares directly (Apple Silicon), so a model in RAM also runs on the GPU.
    pub unified_memory: bool,
    /// The GPUs found; None when they couldn't be detected (recommendations then use RAM only).
    pub gpus: Option<Vec<Gpu>>,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    pub vendor: GpuVendor,
    pub name: String,
    /// Dedicated video memory; 0 when unknown.
    pub vram_gb: f64,
    /// Part of the CPU, sharing system RAM (most Intel graphics, AMD APUs). Models gain little
    /// from running on these, so they don't count as GPU memory.
    pub integrated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GpuVendor { Nvidia, Amd, Intel, Apple, Other }

/// Reads a hardware profile. Each kind of hardware detection is its own implementation.
pub trait Probe {
    /// Missing readings come back as 0 (or empty) rather than failing.
    fn profile(&self) -> HardwareProfile;
}

/// This machine, as the operating system reports it: RAM, OS, CPU architecture, Apple
/// Silicon's unified memory, and GPUs (nvidia-smi everywhere, plus the Windows registry, Linux
/// sysfs or macOS system_profiler for the rest).
pub struct SystemProbe;

impl Probe for SystemProbe {
    fn profile(&self) -> HardwareProfile { probe_system() }
}

/// This machine's profile. Takes a second or two, for the GPU tools (each gets at most 5 s); it
/// isn't run at startup.
pub fn probe() -> HardwareProfile { SystemProbe.profile() }

fn probe_system() -> HardwareProfile {
    let apple_silicon = cfg!(all(target_os = "macos", target_arch = "aarch64"));
    // nvidia-smi is the slowest part: run it alongside the rest
    let nvidia = (!apple_silicon).then(|| std::thread::spawn(nvidia_smi));
    let (total, available) = match sysinfo::System::new() {
        Ok(mut sys) => { sys.refresh_memory(); (sys.total_memory(), sys.available_memory()) }
        Err(_) => (0, 0),
    };
    let gpus = match nvidia {
        None => Some(vec![]), // Apple Silicon: its GPU shares the RAM, which unified_memory covers
        Some(nvidia) => merge_gpus(nvidia.join().ok().flatten(), platform_gpus()),
    };
    HardwareProfile {
        total_ram_gb: gb(total),
        available_ram_gb: gb(available),
        unified_memory: apple_silicon,
        gpus,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
    }
}

/// nvidia-smi knows NVIDIA cards best; the platform's own listing adds every other GPU.
/// None when neither could be read.
fn merge_gpus(nvidia: Option<Vec<Gpu>>, platform: Option<Vec<Gpu>>) -> Option<Vec<Gpu>> {
    if nvidia.is_none() && platform.is_none() { return None; }
    let mut gpus = nvidia.unwrap_or_default();
    let have_nvidia = !gpus.is_empty();
    gpus.extend(platform.unwrap_or_default().into_iter()
        .filter(|g| !(have_nvidia && g.vendor == GpuVendor::Nvidia))
        // virtual and remote-desktop adapters
        .filter(|g| g.vendor != GpuVendor::Other || g.vram_gb > 0.0));
    Some(gpus)
}

fn vendor_of(name: &str) -> GpuVendor {
    let n = name.to_lowercase();
    if ["nvidia", "geforce", "quadro", "tesla"].iter().any(|v| n.contains(v)) { GpuVendor::Nvidia }
    else if n.contains("amd") || n.contains("radeon") { GpuVendor::Amd }
    else if n.contains("intel") { GpuVendor::Intel }
    else { GpuVendor::Other }
}

/// Intel Arc cards: "Arc A770", "Arc(TM) B580" (the built-in "Arc Graphics" has no model number).
static INTEL_CARD: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"arc\S*\s+[ab]\d{3}").expect("valid pattern"));
/// AMD APU graphics: "Radeon(TM) Graphics", "Radeon 780M Graphics", "Radeon(TM) RX Vega 10 Graphics".
static AMD_APU: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"radeon\S*\s+(graphics|\d{3}m\b|rx vega \d+ graphics)").expect("valid pattern")
});

/// Integrated graphics, by name (Windows and Intel Macs report names; Linux reports PCI ids).
fn integrated_by_name(vendor: GpuVendor, name: &str) -> bool {
    let n = name.to_lowercase();
    match vendor {
        GpuVendor::Intel => !INTEL_CARD.is_match(&n),
        GpuVendor::Amd => AMD_APU.is_match(&n),
        GpuVendor::Nvidia | GpuVendor::Apple | GpuVendor::Other => false,
    }
}

/// Run a tool and return its output, or None if it's missing, fails or takes more than 5 s.
fn run(program: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    // bytes, not a String: Windows tools may print in the console code page, not UTF-8
    let mut out = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || { let mut b = vec![]; let _ = out.read_to_end(&mut b); b });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if start.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(20)),
            _ => { let _ = child.kill(); let _ = child.wait(); break None; }
        }
    };
    let bytes = reader.join().ok()?;
    status.filter(|st| st.success()).map(|_| String::from_utf8_lossy(&bytes).into_owned())
}

fn nvidia_smi() -> Option<Vec<Gpu>> {
    run("nvidia-smi", &["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"]).map(|o| parse_nvidia_smi(&o))
}

/// "NVIDIA GeForce RTX 4060 Laptop GPU, 8188" per line; memory in MiB.
fn parse_nvidia_smi(out: &str) -> Vec<Gpu> {
    out.lines().filter_map(|l| {
        let (name, mib) = l.rsplit_once(',')?;
        let mib: f64 = mib.trim().parse().ok()?;
        Some(Gpu { vendor: GpuVendor::Nvidia, name: name.trim().into(), vram_gb: mib_to_gb(mib), integrated: false })
    }).collect()
}

#[cfg(windows)]
fn platform_gpus() -> Option<Vec<Gpu>> {
    // the display adapters' driver entries; qwMemorySize is 64-bit (WMI's AdapterRAM stops at 4 GB).
    // Ask for single values: listing everything under the key takes many seconds.
    const KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let query = |value: &'static str| std::thread::spawn(move || run("reg", &["query", KEY, "/s", "/v", value]));
    let (names, qw, dw) = (query("DriverDesc"), query("HardwareInformation.qwMemorySize"), query("HardwareInformation.MemorySize"));
    // the key keeps entries for adapters that were removed; pnputil lists the ones present
    // (its /connected option needs Windows 10 2004 or later; without it, keep them all)
    let present = std::thread::spawn(|| run("pnputil", &["/enum-devices", "/class", "Display", "/connected"]));
    let names = names.join().ok().flatten()?;
    let sizes: Vec<String> = [qw, dw].into_iter().filter_map(|t| t.join().ok().flatten()).collect();
    let mut gpus = parse_windows_gpus(&names, &sizes.iter().map(String::as_str).collect::<Vec<_>>());
    if let Some(present) = present.join().ok().flatten() {
        let present: Vec<&str> = present.lines().filter_map(|l| l.strip_prefix("Device Description:")).map(str::trim).collect();
        if !present.is_empty() { gpus.retain(|g| present.contains(&g.name.as_str())); }
    }
    Some(gpus)
}

/// Joins `reg query /s /v` listings by registry key: names from one, memory sizes from the others
/// (the first listing that has a size for a key wins).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_windows_gpus(names: &str, sizes: &[&str]) -> Vec<Gpu> {
    let size_of = |key: &str| sizes.iter().find_map(|listing| {
        reg_values(listing).into_iter().find(|(k, _, _)| k == key).and_then(|(_, kind, v)| reg_number(&kind, &v))
    });
    reg_values(names).into_iter().map(|(key, _, name)| {
        let vendor = vendor_of(&name);
        let vram_gb = size_of(&key).map(gb).unwrap_or(0.0);
        Gpu { integrated: integrated_by_name(vendor, &name), vendor, name, vram_gb }
    }).collect()
}

/// (key, type, data) for each value line of `reg query` output.
#[cfg_attr(not(windows), allow(dead_code))]
fn reg_values(out: &str) -> Vec<(String, String, String)> {
    let mut key = String::new();
    let mut found = vec![];
    for line in out.lines() {
        if line.starts_with("HKEY_") { key = line.trim().to_string(); continue; }
        // "    Name    REG_TYPE    data"; the data may contain spaces
        let mut parts = line.trim().splitn(3, "    ");
        if let (Some(_), Some(kind), Some(data)) = (parts.next(), parts.next(), parts.next())
            && kind.starts_with("REG_") { found.push((key.clone(), kind.to_string(), data.trim().to_string())); }
    }
    found
}

/// A size in bytes from a REG_QWORD/REG_DWORD (hex) or a little-endian REG_BINARY.
#[cfg_attr(not(windows), allow(dead_code))]
fn reg_number(kind: &str, data: &str) -> Option<u64> {
    match kind {
        "REG_QWORD" | "REG_DWORD" => u64::from_str_radix(data.trim_start_matches("0x"), 16).ok(),
        "REG_BINARY" if data.len().is_multiple_of(2) && data.len() <= 16 => {
            let bytes = (0..data.len()).step_by(2).map(|i| u8::from_str_radix(&data[i..i + 2], 16).ok()).collect::<Option<Vec<u8>>>()?;
            Some(bytes.iter().rev().fold(0u64, |n, b| n << 8 | *b as u64))
        }
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn platform_gpus() -> Option<Vec<Gpu>> {
    let gpus: Vec<Gpu> = std::fs::read_dir("/sys/class/drm").ok()?.flatten().filter_map(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        // card0, card1, ...; not their connectors (card0-HDMI-A-1)
        if !name.starts_with("card") || name.contains('-') { return None; }
        let dev = e.path().join("device");
        let read = |f: &str| std::fs::read_to_string(dev.join(f)).ok().map(|s| s.trim().to_string());
        Some(linux_gpu(&read("vendor")?, &read("device").unwrap_or_default(),
            read("mem_info_vram_total").and_then(|v| v.parse().ok()), read("product_name")))
    }).collect();
    // nothing readable (a container, a locked-down system): say we couldn't tell
    (!gpus.is_empty()).then_some(gpus)
}

/// A GPU from its sysfs PCI vendor and device ids and, for AMD, its VRAM in bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_gpu(vendor_id: &str, device_id: &str, vram_bytes: Option<u64>, product: Option<String>) -> Gpu {
    let vendor = match vendor_id { "0x10de" => GpuVendor::Nvidia, "0x1002" => GpuVendor::Amd, "0x8086" => GpuVendor::Intel, _ => GpuVendor::Other };
    let vram_gb = vram_bytes.map(gb).unwrap_or(0.0);
    let name = product.filter(|p| !p.is_empty()).unwrap_or_else(|| format!("{vendor:?} GPU"));
    let integrated = match vendor {
        // the slice of RAM set aside for an APU shows up as VRAM. shortcut: sysfs has no "APU"
        // flag, so this goes by size; an APU given 2 GB or more counts as a card
        GpuVendor::Amd => vram_gb < 2.0,
        // Arc cards have PCI device ids 0x56xx (A-series) and 0xe2xx (B-series); sysfs doesn't
        // show their VRAM, so they're listed but not counted
        GpuVendor::Intel => !(device_id.starts_with("0x56") || device_id.starts_with("0xe2")),
        _ => false,
    };
    Gpu { vendor, name, vram_gb, integrated }
}

#[cfg(target_os = "macos")]
fn platform_gpus() -> Option<Vec<Gpu>> {
    run("system_profiler", &["SPDisplaysDataType", "-json"]).map(|o| parse_system_profiler(&o))
}

/// Intel Macs: a discrete GPU reports "spdisplays_vram" ("8 GB"); integrated ones share RAM.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_system_profiler(json: &str) -> Vec<Gpu> {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    v["SPDisplaysDataType"].as_array().into_iter().flatten().map(|d| {
        let name = d["sppci_model"].as_str().unwrap_or("GPU").to_string();
        let vendor = vendor_of(&name);
        let vram_gb = d["spdisplays_vram"].as_str().or(d["_spdisplays_vram"].as_str()).and_then(|s| {
            let (n, unit) = s.trim().split_once(' ')?;
            let n: f64 = n.parse().ok()?;
            Some(mib_to_gb(if unit.eq_ignore_ascii_case("MB") { n } else { n * 1024.0 }))
        }).unwrap_or(0.0);
        // no dedicated memory of its own: built-in graphics sharing RAM
        Gpu { integrated: vram_gb == 0.0 || integrated_by_name(vendor, &name), vendor, name, vram_gb }
    }).collect()
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_gpus() -> Option<Vec<Gpu>> { None }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nvidia_smi() {
        let g = parse_nvidia_smi("NVIDIA GeForce RTX 4060 Laptop GPU, 8188\nNVIDIA RTX A6000, 49140\n");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name, "NVIDIA GeForce RTX 4060 Laptop GPU");
        assert!((g[0].vram_gb - 8.59).abs() < 0.01); // 8188 MiB
        assert!(!g[0].integrated);
    }

    #[test]
    fn reads_the_windows_display_adapters() {
        // real `reg query` output from a laptop with Intel graphics and an RTX 4060
        let k = r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
        let names = format!("\r\n{k}\\0000\r\n    DriverDesc    REG_SZ    Intel(R) UHD Graphics\r\n\r\n{k}\\0001\r\n    DriverDesc    REG_SZ    NVIDIA GeForce RTX 4060 Laptop GPU\r\n\r\nEnd of search: 2 match(es) found.\r\n");
        let qw = format!("{k}\\0001\r\n    HardwareInformation.qwMemorySize    REG_QWORD    0x1ffc00000\r\n\r\nEnd of search: 1 match(es) found.\r\n");
        let dw = format!("{k}\\0000\r\n    HardwareInformation.MemorySize    REG_BINARY    00F0FF7F\r\n{k}\\0001\r\n    HardwareInformation.MemorySize    REG_DWORD    0xfff00000\r\n");
        let g = parse_windows_gpus(&names, &[&qw, &dw]);
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].vendor, g[0].integrated), (GpuVendor::Intel, true));
        assert_eq!((g[1].vendor, g[1].integrated), (GpuVendor::Nvidia, false));
        assert!((g[1].vram_gb - 8.59).abs() < 0.01, "the 64-bit size, not the 4 GB DWORD: {}", g[1].vram_gb);
    }

    #[test]
    fn tells_integrated_graphics_from_cards() {
        let integrated = |name: &str| integrated_by_name(vendor_of(name), name);
        assert!(integrated("Intel(R) UHD Graphics"));
        assert!(integrated("Intel(R) Arc(TM) Graphics")); // Meteor Lake's built-in Arc
        assert!(!integrated("Intel(R) Arc(TM) A770 Graphics"));
        assert!(integrated("AMD Radeon(TM) Graphics"));
        assert!(integrated("AMD Radeon 780M Graphics"));
        assert!(integrated("AMD Radeon(TM) RX Vega 10 Graphics"));
        assert!(!integrated("AMD Radeon RX 7900 XTX"));
        assert!(!integrated("AMD Radeon RX 7600M XT")); // a laptop card, despite the M
        assert!(!integrated("AMD Radeon VII"));
        assert!(!integrated("AMD Radeon Pro 5500M"));
        assert!(!integrated("NVIDIA GeForce RTX 3060"));
    }

    #[test]
    fn reads_linux_sysfs_and_macos_system_profiler() {
        let amd = linux_gpu("0x1002", "0x73bf", Some(16_000_000_000), Some("Radeon RX 6800".into()));
        assert_eq!((amd.vendor, amd.integrated, amd.vram_gb), (GpuVendor::Amd, false, 16.0));
        assert!(linux_gpu("0x1002", "0x15bf", Some(512_000_000), None).integrated); // an APU's carve-out
        assert!(!linux_gpu("0x8086", "0x56a0", None, None).integrated); // Arc A770
        assert!(linux_gpu("0x8086", "0xa7a8", None, None).integrated); // UHD Graphics
        let json = r#"{"SPDisplaysDataType":[{"sppci_model":"Intel UHD Graphics 630"},{"sppci_model":"AMD Radeon Pro 5500M","spdisplays_vram":"8 GB"}]}"#;
        let g = parse_system_profiler(json);
        assert!(g[0].integrated);
        assert_eq!((g[1].vendor, g[1].integrated), (GpuVendor::Amd, false));
        assert!((g[1].vram_gb - 8.59).abs() < 0.01);
    }

    #[test]
    fn nvidia_smi_wins_for_nvidia_and_nothing_readable_means_unknown() {
        let card = |vendor, vram_gb| Gpu { vendor, name: "x".into(), vram_gb, integrated: false };
        let merged = merge_gpus(Some(vec![card(GpuVendor::Nvidia, 8.6)]), Some(vec![card(GpuVendor::Nvidia, 4.0), card(GpuVendor::Intel, 0.0)])).unwrap();
        assert_eq!(merged.iter().map(|g| (g.vendor, g.vram_gb)).collect::<Vec<_>>(), [(GpuVendor::Nvidia, 8.6), (GpuVendor::Intel, 0.0)]);
        assert_eq!(merge_gpus(None, None), None);
        assert_eq!(merge_gpus(None, Some(vec![])), Some(vec![]));
    }
}
