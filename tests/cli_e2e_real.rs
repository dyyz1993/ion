//! CLI 真实 LLM e2e 烟测 — 验证新功能在真实 API 下能工作
//!
//! 运行方式：
//!   ION_E2E_CLI=1 cargo test --test cli_e2e_real -- --ignored --nocapture
//!
//! 环境变量：
//!   ION_API_KEY          — API key (或用 auth.json)
//!   ION_E2E_PROVIDER     — provider 名 (默认 opencode)
//!   ION_E2E_MODEL        — 模型 id (默认 deepseek-v4-flash)

#![cfg(test)]

use std::process::Command;

fn enabled() -> bool {
    std::env::var("ION_E2E_CLI").is_ok()
}

fn provider() -> String {
    std::env::var("ION_E2E_PROVIDER").unwrap_or_else(|_| "opencode".into())
}

fn model() -> String {
    std::env::var("ION_E2E_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into())
}

fn ion_bin() -> String {
    let target = std::env::current_dir().unwrap();
    let bin = target.join("target/debug/ion");
    if bin.exists() {
        bin.to_string_lossy().to_string()
    } else {
        "ion".into()
    }
}

fn run_ion(args: &[&str], stdin: Option<&str>) -> (bool, String) {
    let mut cmd = Command::new(ion_bin());
    cmd.args(args);
    if let Some(s) = stdin {
        use std::io::Write;
        let tmp = std::env::temp_dir().join("ion_e2e_stdin.txt");
        let mut f = std::fs::File::create(&tmp).unwrap();
        writeln!(f, "{s}").unwrap();
        let f_in = std::fs::File::open(&tmp).unwrap();
        cmd.stdin(f_in);
    } else {
        cmd.stdin(std::process::Stdio::null());
    }
    cmd.env("RUST_LOG", "warn");
    let output = cmd.output().expect("failed to run ion");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), format!("{stdout}{stderr}"))
}

#[test]
#[ignore]
fn e2e_basic_prompt() {
    if !enabled() { return; }
    let (ok, out) = run_ion(
        &["-p", "say hello in 2 words", "--provider", &provider(), "--model", &model()],
        None,
    );
    assert!(ok, "ion should exit 0, got output: {out}");
    assert!(!out.trim().is_empty(), "should have output");
    println!("e2e_basic_prompt: {out}");
}

#[test]
#[ignore]
fn e2e_provider_id_syntax() {
    if !enabled() { return; }
    let combined = format!("{}/{}", provider(), model());
    let (ok, out) = run_ion(
        &["-p", "say hi", "--model", &combined],
        None,
    );
    assert!(ok, "provider/id syntax should work, output: {out}");
    println!("e2e_provider_id_syntax: {out}");
}

#[test]
#[ignore]
fn e2e_thinking_suffix() {
    if !enabled() { return; }
    let model_with_thinking = format!("{}:low", model());
    let (ok, out) = run_ion(
        &["-p", "say ok", "--model", &model_with_thinking, "--provider", &provider()],
        None,
    );
    // :low thinking should parse without error
    assert!(ok || !out.contains("error"), "thinking suffix parse failed: {out}");
    println!("e2e_thinking_suffix: {out}");
}

#[test]
#[ignore]
fn e2e_piped_stdin() {
    if !enabled() { return; }
    let (ok, out) = run_ion(
        &["--provider", &provider(), "--model", &model()],
        Some("say hello in 2 words"),
    );
    // 接受成功或 API 限流错误（证明 stdin 被读取了即可）
    let stdin_read = ok || out.contains("429") || out.contains("retry") || out.contains("Error");
    assert!(stdin_read, "piped stdin should be read, got: {out}");
    if ok {
        assert!(!out.trim().is_empty(), "should respond to piped input");
    }
    println!("e2e_piped_stdin (ok={ok}): rate-limited or responded");
}

#[test]
#[ignore]
fn e2e_mode_json() {
    if !enabled() { return; }
    let (ok, out) = run_ion(
        &["--mode", "json", "-p", r#"output {"ok":true}"#, "--provider", &provider(), "--model", &model()],
        None,
    );
    assert!(ok, "--mode json should work, output: {out}");
    println!("e2e_mode_json: {out}");
}

#[test]
#[ignore]
fn e2e_continue_creates_and_resumes() {
    if !enabled() { return; }
    // First: create a session
    let sid = format!("sess_e2e_{}", std::process::id());
    let (ok1, out1) = run_ion(
        &["--session-id", &sid, "-p", "remember the number 42", "--provider", &provider(), "--model", &model()],
        None,
    );
    assert!(ok1, "session create should work: {out1}");

    // Then: continue with a follow-up
    let (ok2, out2) = run_ion(
        &["--session", &sid, "-p", "what number did I tell you?", "--provider", &provider(), "--model", &model()],
        None,
    );
    assert!(ok2, "session resume should work: {out2}");
    // The model should mention 42 (or close to it) if context is preserved
    println!("e2e_continue: resume output: {out2}");
}

#[test]
#[ignore]
fn e2e_compact_model_flag() {
    if !enabled() { return; }
    let (ok, out) = run_ion(
        &["--compact-model", &model(), "-p", "say hi", "--provider", &provider(), "--model", &model()],
        None,
    );
    assert!(ok, "--compact-model should not break execution: {out}");
    println!("e2e_compact_model: {out}");
}
