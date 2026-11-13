//! Shared by the integration tests: the scripted fake model server (tests/mock_server.py) and a
//! temporary Cortex home folder.
#![allow(dead_code)] // each test file uses part of this

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Once;

/// A fake model server on a free port, stopped when dropped.
pub struct MockServer { child: Child, pub port: u16 }

impl MockServer {
    pub fn start() -> MockServer { MockServer::start_on(0) }

    /// On `port` (0 picks a free one), e.g. to bring a stopped server back where it was.
    pub fn start_on(port: u16) -> MockServer {
        // tests/ at the repo root, found from whichever package's tests use this
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|d| d.join("tests/mock_server.py"))
            .find(|p| p.exists()).expect("tests/mock_server.py in the repo");
        // python3 first; on Windows it can be a Store placeholder that prints nothing, so fall through
        for py in ["python3", "python"] {
            let Ok(mut child) = Command::new(py).arg("-u").arg(&script).arg(port.to_string()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { continue };
            let mut line = String::new();
            let _ = BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line);
            match line.trim().strip_prefix("PORT ").and_then(|p| p.parse().ok()) {
                Some(port) => return MockServer { child, port },
                None => { let _ = child.kill(); let _ = child.wait(); }
            }
        }
        panic!("python is needed to run the fake model server (tests/mock_server.py)")
    }

    pub fn url(&self) -> String { format!("http://127.0.0.1:{}", self.port) }

    /// "Download" a model on it, as `ollama pull` would.
    pub fn install(&self, model: &str) {
        let mut r = ureq::post(&format!("{}/api/pull", self.url())).send_json(serde_json::json!({"model": model})).unwrap().into_reader();
        std::io::copy(&mut r, &mut std::io::sink()).unwrap();
    }
}

impl Drop for MockServer {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

/// Sessions save under the Cortex home folder: keep them in a temporary one.
pub fn temp_home() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let home = std::env::temp_dir().join(format!("cortex_tests{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: set once, before any test reads the environment (every test calls this first)
        unsafe { std::env::set_var("CORTEX_HOME", home) };
    });
}

