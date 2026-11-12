//! What this machine has to run a model with.

/// The hardware a recommendation is made for. Sizes are in GB (10^9 bytes, like model downloads).
#[derive(Debug, Clone, PartialEq)]
pub struct HardwareProfile {
    pub total_ram_gb: f64,
    /// RAM free right now (other programs come and go, so this is only a hint).
    pub available_ram_gb: f64,
    /// RAM the GPU shares directly (Apple Silicon), so a model in RAM also runs on the GPU.
    pub unified_memory: bool,
    pub os: String,
    pub arch: String,
}

/// Reads a hardware profile. Each kind of hardware detection is its own implementation.
pub trait Probe {
    /// Missing readings come back as 0 (or empty) rather than failing.
    fn profile(&self) -> HardwareProfile;
}

/// This machine, as the operating system reports it: RAM, OS, CPU architecture and Apple
/// Silicon's unified memory.
pub struct SystemProbe;

impl Probe for SystemProbe {
    fn profile(&self) -> HardwareProfile { probe_system() }
}

/// This machine's profile.
pub fn probe() -> HardwareProfile { SystemProbe.profile() }

fn probe_system() -> HardwareProfile {
    let gb = |bytes: u64| bytes as f64 / 1e9;
    let (total, available) = match sysinfo::System::new() {
        Ok(mut sys) => { sys.refresh_memory(); (sys.total_memory(), sys.available_memory()) }
        Err(_) => (0, 0),
    };
    HardwareProfile {
        total_ram_gb: gb(total),
        available_ram_gb: gb(available),
        unified_memory: cfg!(all(target_os = "macos", target_arch = "aarch64")),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
    }
}
