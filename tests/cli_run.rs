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
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn doctor_says_whether_semantic_recall_is_on() {
    let server = common::MockServer::start();
    let home = std::env::temp_dir().join(format!("cortex_cli_doctor_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), format!("backend = \"ollama\"\nbase_url = \"{}\"\nmodel = \"mock:7b\"\n", server.url())).unwrap();
    let doctor = || {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_cortex")).arg("doctor").env("CORTEX_HOME", &home).env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null()).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let out = doctor();
    assert!(out.contains("semantic recall off") && out.contains("ollama pull nomic-embed-text"), "{out}");
    server.install("nomic-embed-text");
    let out = doctor();
    assert!(out.contains("semantic recall on (nomic-embed-text)"), "{out}");
    // an OpenAI-compatible server gets its own hint
    let fresh = common::MockServer::start(); // one with no embedding model
    std::fs::write(home.join("config.toml"), format!("backend = \"openai\"\nbase_url = \"{}/v1\"\nmodel = \"mock-7b\"\n", fresh.url())).unwrap();
    let out = doctor();
    assert!(out.contains("load an embedding model in your server"), "{out}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn piped_input_that_cannot_be_read_stops_the_run_with_a_reason() {
    use std::io::Write;
    let home = std::env::temp_dir().join(format!("cortex_cli_badpipe_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_cortex")).args(["run", "summarize this"]).env("CORTEX_HOME", &home)
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"caf\xe9 au lait\n").unwrap(); // Latin-1, not UTF-8
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("couldn't read the piped input"), "{err}");
    let _ = std::fs::remove_dir_all(&home);
}
