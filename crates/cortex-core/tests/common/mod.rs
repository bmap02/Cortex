//! Shared by the integration tests: the scripted fake model server (tests/mock_server.py) and a
//! temporary Cortex home folder.
#![allow(dead_code)] // each test file uses part of this

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Once;

/// A fake model server on a free port, stopped when dropped.
pub struct MockServer { child: Child, pub port: u16 }

impl MockServer {
    pub fn start() -> MockServer {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/mock_server.py");
        // python3 first; on Windows it can be a Store placeholder that prints nothing, so fall through
        for py in ["python3", "python"] {
            let Ok(mut child) = Command::new(py).args(["-u", script, "0"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { continue };
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

