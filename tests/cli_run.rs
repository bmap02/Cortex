//! `cortex run` as a user runs it, against the scripted fake model server.

#[path = "../crates/cortex-core/tests/common/mod.rs"]
mod common;

#[test]
fn memories_suggested_in_a_one_shot_run_are_left_with_a_way_to_review_them() {
    let server = common::MockServer::start();
    let home = std::env::temp_dir().join(format!("cortex_cli_run_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), format!("backend = \"ollama\"\nbase_url = \"{}\"\nmodel = \"mock:7b\"\n", server.url())).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cortex"))
        .args(["run", "-q", "I learned that the user prefers tabs"]).env("CORTEX_HOME", &home).env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null()).output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("1 suggested memory") && stderr.contains("cortex -r "), "told where to review it: {stderr}");
    assert!(!stdout.contains("suggested memory"), "the answer on stdout stays clean for pipes: {stdout}");
}
