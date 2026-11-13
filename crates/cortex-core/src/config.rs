//! Config lives in ~/.cortex/config.toml (override the folder with CORTEX_HOME).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Ollama's native API (/api/chat)
    Ollama,
    /// Any OpenAI-compatible server: llama.cpp server, LM Studio, vLLM, LocalAI, Jan, ...
    Openai,
}

impl BackendKind {
    pub fn default_url(self) -> &'static str {
        match self {
            BackendKind::Ollama => "http://localhost:11434",
            BackendKind::Openai => "http://localhost:8080/v1",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Ollama => "ollama",
            BackendKind::Openai => "openai-compatible",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub backend: BackendKind,
    pub base_url: String,
    pub model: String,
    /// Name of an environment variable holding an API key (only if your server needs one).
    pub api_key_env: String,
    pub num_ctx: usize,
    pub temperature: f32,
    /// Ollama "thinking" for models that support it: true, false, or unset (the model's default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
    /// Max tool-call rounds per message before the agent stops.
    pub max_steps: usize,
    /// Skip the y/n prompt before shell commands and file writes.
    pub auto_approve: bool,
    /// Lets the fetch_url tool reach the network. Off by default: Cortex is offline-first.
    pub allow_network: bool,
    pub show_banner: bool,
    /// Tools to turn off, e.g. ["run_shell"].
    pub disabled_tools: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend: BackendKind::Ollama,
            base_url: BackendKind::Ollama.default_url().into(),
            model: String::new(),
            api_key_env: String::new(),
            num_ctx: 8192,
            temperature: 0.7,
            think: None,
            max_steps: 12,
            auto_approve: false,
            allow_network: false,
            show_banner: true,
            disabled_tools: vec![],
        }
    }
}

/// The user's home folder: $HOME on macOS/Linux, %USERPROFILE% on Windows.
pub fn user_home() -> Option<PathBuf> {
    let vars: &[&str] = if cfg!(windows) { &["USERPROFILE", "HOME"] } else { &["HOME", "USERPROFILE"] };
    vars.iter().filter_map(|v| std::env::var(v).ok()).find(|v| !v.is_empty()).map(PathBuf::from)
}

/// ~/.cortex (or %USERPROFILE%\.cortex on Windows). Override with CORTEX_HOME.
pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("CORTEX_HOME") {
        return PathBuf::from(h);
    }
    user_home().unwrap_or_else(|| PathBuf::from(".")).join(".cortex")
}

/// "32k", "32768", "128K", "1m" -> tokens. Minimum 512.
pub fn parse_tokens(s: &str) -> Result<usize, String> {
    let t = s.trim().to_lowercase().replace([',', '_'], "");
    let (num, mult) = if let Some(n) = t.strip_suffix('k') { (n, 1024.0) }
        else if let Some(n) = t.strip_suffix('m') { (n, 1024.0 * 1024.0) }
        else { (t.as_str(), 1.0) };
    let v: f64 = num.trim().parse().map_err(|_| format!("'{s}' isn't a token count (try 16384 or 32k)"))?;
    let n = (v * mult).round() as usize;
    if n < 512 { return Err("context must be at least 512 tokens".into()); }
    Ok(n)
}

/// Desktop / Documents / Downloads, following OneDrive redirection on Windows.
pub fn user_folder(name: &str) -> Option<PathBuf> {
    let home = user_home()?;
    let mut candidates = vec![];
    if cfg!(windows) {
        for var in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
            if let Ok(od) = std::env::var(var) { candidates.push(PathBuf::from(od).join(name)); }
        }
        candidates.push(home.join("OneDrive").join(name));
    }
    candidates.push(home.join(name));
    candidates.into_iter().find(|p| p.is_dir())
}

pub fn config_path() -> PathBuf { home().join("config.toml") }
pub fn sessions_dir() -> PathBuf { home().join("sessions") }
/// The model catalog `cortex update` downloaded (used when it's newer than the bundled one).
pub fn catalog_path() -> PathBuf { home().join("catalog.toml") }
pub fn soul_path() -> PathBuf { home().join("SOUL.md") }
pub fn history_path() -> PathBuf { home().join("history.txt") }

impl Config {
    pub fn exists() -> bool { config_path().exists() }

    pub fn load() -> Config {
        Config::read_from(&config_path()).unwrap_or_else(|e| {
            eprintln!("warning: {e}. Using defaults.");
            Config::default()
        })
    }

    /// The settings saved at `path` (defaults if there's no file). A file that exists but
    /// can't be read is an error, so nothing overwrites it with defaults.
    fn read_from(path: &std::path::Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("couldn't parse {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(home())?;
        let s = toml::to_string_pretty(self).expect("config serializes");
        std::fs::write(config_path(), s)
    }

    /// Change the saved settings. Works on a fresh copy of the file, so one-off overrides
    /// for this run (`--model`, `--ctx`) are never written to it.
    pub fn update(change: impl FnOnce(&mut Config) -> Result<(), String>) -> Result<(), String> {
        let mut saved = Config::read_from(&config_path()).map_err(|e| format!("{e}; fix or delete it before changing settings"))?;
        change(&mut saved)?;
        saved.save().map_err(|e| format!("couldn't save {}: {e}", config_path().display()))
    }

    pub fn api_key(&self) -> Option<String> {
        if self.api_key_env.is_empty() { return None; }
        std::env::var(&self.api_key_env).ok().filter(|k| !k.is_empty())
    }

    /// `cortex config set <key> <value>`
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let b = |v: &str| v.parse::<bool>().map_err(|_| format!("{key} expects true/false"));
        let n = |v: &str| v.parse::<usize>().map_err(|_| format!("{key} expects a number"));
        match key {
            "backend" => {
                self.backend = match value {
                    "ollama" => BackendKind::Ollama,
                    "openai" | "openai-compatible" => BackendKind::Openai,
                    _ => return Err("backend must be ollama or openai".into()),
                }
            }
            "base_url" => self.base_url = value.trim_end_matches('/').into(),
            "model" => self.model = value.into(),
            "api_key_env" => self.api_key_env = value.into(),
            "num_ctx" | "context" => self.num_ctx = parse_tokens(value)?,
            "temperature" => self.temperature = value.parse().map_err(|_| "temperature expects a number")?,
            "think" => self.think = match value {
                "true" | "on" => Some(true),
                "false" | "off" => Some(false),
                "auto" | "default" => None,
                _ => return Err("think expects true, false or auto".into()),
            },
            "max_steps" => self.max_steps = n(value)?,
            "auto_approve" => self.auto_approve = b(value)?,
            "allow_network" => self.allow_network = b(value)?,
            "show_banner" => self.show_banner = b(value)?,
            _ => return Err(format!("unknown key '{key}'")),
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        if key == "think" && self.think.is_none() { return Some("auto".into()); }
        let v = toml::Value::try_from(self).ok()?;
        v.get(key).map(|x| match x {
            toml::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn unreadable_config_is_an_error_not_defaults() {
        let dir = std::env::temp_dir().join(format!("cortex_cfg{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // no file yet: defaults
        assert_eq!(Config::read_from(&path).unwrap().num_ctx, Config::default().num_ctx);
        // a valid file is read
        std::fs::write(&path, "num_ctx = 16384\n").unwrap();
        assert_eq!(Config::read_from(&path).unwrap().num_ctx, 16384);
        // a typo is reported, so update() won't replace the file with defaults
        std::fs::write(&path, "num_ctx = \n").unwrap();
        assert!(Config::read_from(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
