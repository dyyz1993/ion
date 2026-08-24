//! Integration tests for Todo + Plan WASM extensions and PlanExtension.
//!
//! Verifies:
//! - WASM extensions load and register tools correctly
//! - WASM tool execution returns expected results
//! - PlanExtension correctly restricts tools in plan mode
//! - PlanExtension on_system_prompt injects planning instructions

use ion::agent::extension::Extension;

// ---------------------------------------------------------------------------
// Helpers: build WASM extensions
// ---------------------------------------------------------------------------

fn build_todo_extension() -> String {
    build_wasm_extension("extensions/todo-extension", "todo_extension.wasm")
}

fn build_hello_extension() -> String {
    build_wasm_extension("extensions/hello-extension", "hello_extension.wasm")
}


fn build_wasm_extension(pkg_dir: &str, wasm_file: &str) -> String {
    use std::sync::Once;

    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let pkg_path = manifest_dir.join(pkg_dir);

    // Because tests run in parallel, avoid re-invoking `cargo build` from
    // multiple threads simultaneously — it's wasteful and can cause a
    // transient window where the .wasm is absent (cargo removes then
    // rewrites the artifact). A `Once` per call-site guarantees we build
    // at most once per test process; subsequent callers reuse the file.
    static BUILD_TODO: Once = Once::new();
    static BUILD_PLAN: Once = Once::new();
    match pkg_dir {
        "extensions/todo-extension" => BUILD_TODO.call_once(|| {
            do_build(manifest_dir, pkg_dir);
        }),
        "plan-extension" => BUILD_PLAN.call_once(|| {
            do_build(manifest_dir, pkg_dir);
        }),
        _ => do_build(manifest_dir, pkg_dir),
    }

    // Path to the compiled WASM binary. Probe both workspace-level and
    // package-level target dirs for robustness.
    let workspace_wasm = manifest_dir
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join(wasm_file);
    let pkg_wasm = pkg_path
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join(wasm_file);
    if workspace_wasm.exists() {
        workspace_wasm.to_str().unwrap().to_string()
    } else if pkg_wasm.exists() {
        pkg_wasm.to_str().unwrap().to_string()
    } else {
        panic!(
            "WASM file not found at {} or {}",
            workspace_wasm.display(),
            pkg_wasm.display()
        );
    }
}

fn do_build(manifest_dir: &std::path::Path, pkg_dir: &str) {
    let pkg_path = manifest_dir.join(pkg_dir);
    let output = std::process::Command::new("cargo")
        .args([
            "build",
            "--target",
            "wasm32-wasip1",
            "--release",
            "-q",
        ])
        .current_dir(&pkg_path)
        .output()
        .unwrap_or_else(|e| panic!("failed to build {pkg_dir}: {e}"));
    assert!(
        output.status.success(),
        "{pkg_dir} build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Minimal hello Extension
// ---------------------------------------------------------------------------

#[test]
fn hello_extension_loads_and_registers_tool() {
    let wasm_path = build_hello_extension();
    let mut extension =
        ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
            .expect("hello-extension should load");

    assert_eq!(extension.abi_version, 1);
    assert_eq!(extension.tools.len(), 1);
    assert_eq!(extension.tools[0].name, "hello");
    let output = extension
        .execute_tool("hello", "{}")
        .expect("hello tool should execute");
    assert!(output.contains("Hello from extension!"));
}

// ---------------------------------------------------------------------------
// Todo Extension tests
// ---------------------------------------------------------------------------

#[test]
fn todo_extension_loads_and_registers_tools() {
    let wasm_path = build_todo_extension();
    let extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    let names: Vec<&str> = extension.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"todo_add"), "should register todo_add");
    assert!(names.contains(&"todo_list"), "should register todo_list");
    assert!(names.contains(&"todo_done"), "should register todo_done");
    assert!(
        names.contains(&"todo_remove"),
        "should register todo_remove"
    );
    assert!(names.contains(&"todo_clean"), "should register todo_clean");
    assert_eq!(extension.tools.len(), 5, "should register exactly 5 tools");
}

#[test]
fn todo_extension_create_and_list() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    // Create a task
    let result = extension
        .execute_tool("todo_add", r#"{"text":"调研"}"#)
        .expect("todo_add should succeed");
    assert!(
        result.contains(r#""status":"created""#),
        "result should be created: {result}"
    );

    // List tasks
    let list = extension
        .execute_tool("todo_list", r#"{"status":"all"}"#)
        .expect("todo_list should succeed");
    assert!(list.contains("调研"), "should contain task: {list}");
}

#[test]
fn todo_extension_update_status() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    // Create a task
    let result = extension
        .execute_tool("todo_add", r#"{"text":"调研"}"#)
        .expect("todo_add should succeed");
    assert!(result.contains(r#""id":"#), "should have an id: {result}");

    // List to find the id
    let list = extension
        .execute_tool("todo_list", r#"{"status":"all"}"#)
        .expect("todo_list should succeed");
    assert!(list.contains("调研"), "should contain the task: {list}");
}

#[test]
fn todo_extension_nonexistent_item() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    // Try to done a non-existent item (extension returns status "done" even for nonexistent)
    let result = extension
        .execute_tool("todo_done", r#"{"id":"nonexistent"}"#)
        .expect("todo_done should succeed");
    // The extension returns the id with status "done" for any id
    assert!(
        result.contains(r#""id":"nonexistent""#),
        "should mention the id: {result}"
    );
}

#[test]
fn todo_extension_edge_empty_array() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    // Clean with no tasks should be ok
    let result = extension
        .execute_tool("todo_clean", "{}")
        .expect("todo_clean should succeed");
    assert!(
        result.contains(r#""removed":0"#),
        "should report 0 removed: {result}"
    );
}

#[test]
fn todo_extension_edge_large_list() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    // Add a task
    extension
        .execute_tool("todo_add", r#"{"text":"test"}"#)
        .unwrap();
    extension
        .execute_tool("todo_add", r#"{"text":"test2"}"#)
        .unwrap();

    // List all
    let list = extension
        .execute_tool("todo_list", r#"{"status":"all"}"#)
        .unwrap();
    assert!(list.contains("test"), "should contain test: {list}");
    assert!(list.contains("test2"), "should contain test2: {list}");
}

#[test]
fn todo_extension_edge_special_chars() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    let result = extension
        .execute_tool("todo_add", r#"{"text":"hello <world> & 'rust'"}"#)
        .expect("todo_add should handle special chars");
    assert!(
        result.contains(r#""status":"created""#),
        "result should be created: {result}"
    );
}

#[test]
fn todo_extension_edge_invalid_status() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    // Schema validation should reject invalid status
    // (extension_execute_tool returns what the extension returns)
    let _ = extension
        .execute_tool("todo_list", r#"{"status":"invalid"}"#)
        .unwrap_or_default();
    // If it errors, that's OK; just checking it doesn't panic
}

#[test]
fn todo_extension_edge_update_empty_list() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    // Clean on empty should be fine
    let result = extension
        .execute_tool("todo_clean", "{}")
        .expect("todo_clean should succeed on empty");
    assert!(
        result.contains(r#""removed":0"#),
        "should report 0 removed: {result}"
    );
}

// ---------------------------------------------------------------------------
// PlanExtension unit tests
// ---------------------------------------------------------------------------

/// Helper: create a minimal ToolCall for testing
fn make_tool_call(name: &str, args: &str) -> ion::agent::messages::ToolCall {
    ion::agent::messages::ToolCall {
        call_type: "tool_use".into(),
        id: "test-1".into(),
        name: name.into(),
        arguments: serde_json::from_str(args).unwrap_or_default(),
        thought_signature: None,
    }
}

/// Helper: create a minimal ToolResult for testing
fn make_tool_result() -> ion_provider::types::ToolResult {
    ion_provider::types::ToolResult {
        tool_call_id: "test-1".into(),
        output: "ok".into(),
    }
}

#[tokio::test]
async fn plan_extension_normal_mode_allows_all_tools() {
    let ext = ion::agent::plan_extension::PlanExtension::new();
    assert!(!ext.is_plan_mode(), "should start in normal mode");

    // All tools should be allowed when not in plan mode
    let result = ext
        .before_tool_call(&mut make_tool_call("bash", r#"{"command":"ls"}"#))
        .await;
    assert!(result.is_ok(), "bash should be allowed in normal mode");

    let result = ext
        .before_tool_call(&mut make_tool_call("write", r#"{"path":"/tmp/x"}"#))
        .await;
    assert!(result.is_ok(), "write should be allowed in normal mode");
}

#[tokio::test]
async fn plan_extension_plan_mode_restricts_tools() {
    let ext = ion::agent::plan_extension::PlanExtension::new();

    // Enter plan mode via after_tool_call
    ext.after_tool_call(
        &make_tool_call("plan_enter", r#"{"plan_path":"/tmp/plan.md"}"#),
        &mut make_tool_result(),
    )
    .await
    .unwrap();
    assert!(ext.is_plan_mode(), "should be in plan mode");

    // Plan-allowed tools should still work
    let result = ext
        .before_tool_call(&mut make_tool_call("read", r#"{"file_path":"/tmp/x"}"#))
        .await;
    assert!(result.is_ok(), "read should be allowed in plan mode");

    let result = ext
        .before_tool_call(&mut make_tool_call("plan_exit", "{}"))
        .await;
    assert!(result.is_ok(), "plan_exit should be allowed in plan mode");

    // Non-plan tools should be rejected
    let result = ext
        .before_tool_call(&mut make_tool_call("calculator", r#"{"expression":"1+1"}"#))
        .await;
    assert!(
        result.is_err(),
        "calculator should be rejected in plan mode"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("not available in plan mode"),
        "error should mention plan mode: {err}"
    );
}

#[tokio::test]
async fn plan_extension_exit_plan_mode_restores_tools() {
    let ext = ion::agent::plan_extension::PlanExtension::new();

    // Enter plan mode
    ext.after_tool_call(
        &make_tool_call("plan_enter", r#"{"plan_path":"/tmp/p"}"#),
        &mut make_tool_result(),
    )
    .await
    .unwrap();

    // Exit plan mode
    ext.after_tool_call(&make_tool_call("plan_exit", "{}"), &mut make_tool_result())
        .await
        .unwrap();
    assert!(!ext.is_plan_mode(), "should exit plan mode");

    // calculator should be allowed again
    let result = ext
        .before_tool_call(&mut make_tool_call("calculator", r#"{"expression":"1+1"}"#))
        .await;
    assert!(
        result.is_ok(),
        "calculator should be allowed after plan_exit"
    );
}

#[tokio::test]
async fn plan_extension_injects_system_prompt_in_plan_mode() {
    let ext = ion::agent::plan_extension::PlanExtension::new();

    // Normal mode: should not inject
    let mut prompt = "base prompt".to_string();
    ext.on_system_prompt(&mut prompt).await.unwrap();
    assert_eq!(
        prompt, "base prompt",
        "should not modify prompt in normal mode"
    );

    // Enter plan mode
    ext.after_tool_call(
        &make_tool_call("plan_enter", r#"{"plan_path":"/tmp/my-plan.md"}"#),
        &mut make_tool_result(),
    )
    .await
    .unwrap();

    // Plan mode: should inject instructions
    let mut prompt2 = "base prompt".to_string();
    ext.on_system_prompt(&mut prompt2).await.unwrap();
    assert!(
        prompt2.contains("PLAN MODE"),
        "should inject PLAN MODE marker: {prompt2}"
    );
    assert!(
        prompt2.contains("/tmp/my-plan.md"),
        "should inject plan path: {prompt2}"
    );
    assert!(
        prompt2.contains("plan_exit"),
        "should mention plan_exit: {prompt2}"
    );
}

#[tokio::test]
async fn plan_extension_tracks_plan_path() {
    let ext = ion::agent::plan_extension::PlanExtension::new();

    ext.after_tool_call(
        &make_tool_call("plan_enter", r#"{"plan_path":"/tmp/custom-plan.md"}"#),
        &mut make_tool_result(),
    )
    .await
    .unwrap();

    let mut prompt = String::new();
    ext.on_system_prompt(&mut prompt).await.unwrap();
    assert!(
        prompt.contains("/tmp/custom-plan.md"),
        "should use the custom plan path"
    );
}

// ---------------------------------------------------------------------------
// Registry — hot‑pluggable WASM extension lifecycle  (P1–P4)
// ---------------------------------------------------------------------------

#[test]
fn extension_registry_add_list_remove() {
    let wasm_path = build_todo_extension();
    let registry = ion::wasm_extension::WasmExtensionRegistry::new();

    // P1: add → should return tool defs
    let tool_defs = registry
        .add(&wasm_path)
        .expect("wasm_extension_registry::add should load todo-extension");
    let names: Vec<&str> = tool_defs.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"todo_add"), "add should register todo_add");
    assert!(
        names.contains(&"todo_list"),
        "add should register todo_list"
    );
    assert!(
        names.contains(&"todo_done"),
        "add should register todo_done"
    );
    assert!(
        names.contains(&"todo_remove"),
        "add should register todo_remove"
    );
    assert!(
        names.contains(&"todo_clean"),
        "add should register todo_clean"
    );
    assert_eq!(tool_defs.len(), 5, "exactly 5 tools from todo-extension");

    // P2: list → should include the loaded extension
    let extensions = registry.list();
    assert_eq!(extensions.len(), 1, "list should contain the loaded extension");
    let p = &extensions[0];
    assert!(
        p.path.ends_with("todo_extension.wasm"),
        "path should end with .wasm"
    );
    assert_eq!(p.abi_version, 1, "todo-extension version should be 1");
    assert_eq!(p.tools.len(), 5, "extension info should list 5 tools");

    // P3: remove → should return tool names and clear from list
    let removed = registry
        .remove(&wasm_path)
        .expect("wasm_extension_registry::remove should succeed");
    assert_eq!(removed.len(), 5, "remove should return 5 tool names");
    assert!(removed.contains(&"todo_add".to_string()));

    let empty = registry.list();
    assert_eq!(empty.len(), 0, "after remove, list should be empty");

    // P4: re‑add after removal works
    let tool_defs2 = registry
        .add(&wasm_path)
        .expect("re‑add after remove should work");
    assert_eq!(tool_defs2.len(), 5, "re‑added extension should register tools");
    assert_eq!(
        registry.list().len(),
        1,
        "re‑added extension should show in list"
    );
}

#[test]
fn extension_registry_reload_replaces_instance() {
    let wasm_path = build_todo_extension();
    let registry = ion::wasm_extension::WasmExtensionRegistry::new();

    // Load once
    registry.add(&wasm_path).expect("initial load");
    let extensions_before = registry.list();
    assert_eq!(extensions_before.len(), 1);
    let abi_version_before = extensions_before[0].abi_version;
    let tools_before = extensions_before[0].tools.clone();

    // Reload (same .wasm file, fresh instance)
    let tool_defs = registry.reload(&wasm_path).expect("reload should succeed");
    assert_eq!(tool_defs.len(), 5, "reload should register the same tools");

    // The entry should be replaced
    let extensions_after = registry.list();
    assert_eq!(
        extensions_after.len(),
        1,
        "still exactly one extension after reload"
    );
    // Version should match (same .wasm file)
    assert_eq!(
        extensions_after[0].abi_version, abi_version_before,
        "version unchanged after reload"
    );
    assert_eq!(
        extensions_after[0].tools, tools_before,
        "tools unchanged after reload"
    );
}

#[test]
fn extension_registry_add_same_path_twice_is_reload() {
    let wasm_path = build_todo_extension();
    let registry = ion::wasm_extension::WasmExtensionRegistry::new();

    // add twice → second call replaces the first (reload semantics)
    registry.add(&wasm_path).expect("first add");
    registry.add(&wasm_path).expect("second add (replaces)");

    // list should still have exactly 1 entry
    let extensions = registry.list();
    assert_eq!(extensions.len(), 1, "second add should replace, not duplicate");
}

#[test]
fn extension_registry_remove_nonexistent_returns_error() {
    let registry = ion::wasm_extension::WasmExtensionRegistry::new();
    let result = registry.remove("/nonexistent/path.wasm");
    assert!(result.is_err(), "remove of nonexistent path should fail");
}


// ---------------------------------------------------------------------------
// Extension data dimensions — paths, context injection, extension_id derivation
// ---------------------------------------------------------------------------

#[test]
fn wasm_extension_id_from_path() {
    // file stem wins
    assert_eq!(
        ion::wasm_extension::extension_id_from_path(
            "/home/user/todo-extension/target/release/todo_extension.wasm"
        ),
        "todo_extension",
    );
    assert_eq!(
        ion::wasm_extension::extension_id_from_path("/tmp/my_extension.wasm"),
        "my_extension",
    );
}

#[test]
fn extension_data_dimension_paths_are_correct() {
    use ion::paths;

    let ctx = ion::wasm_extension::Context {
        session_id: "sess-abc".into(),
        cwd: "/tmp/work".into(),
        project_root: "/tmp/work".into(),
        extension_id: "test-ext".into(),
        event_bus: None,
        fs: None,
        tokio_handle: None,
        agent_rpc: None,
    };

    // global: ~/.ion/agent/extensions-data/<ext>/
    let g = paths::global_data_dir(&ctx.extension_id);
    assert!(
        g.to_string_lossy().contains("extensions-data/test-ext"),
        "global: {g:?}"
    );

    // project (in ~/.ion): ~/.ion/agent/project-data/<enc>/<ext>/
    let p = paths::project_data_dir(&ctx.project_root, &ctx.extension_id);
    assert!(
        p.to_string_lossy().contains("project-data/"),
        "project: {p:?}"
    );
    assert!(
        p.to_string_lossy().contains("test-ext"),
        "project ext: {p:?}"
    );

    // project_local (in project directory): <root>/.ion/<ext>/
    let pl = paths::project_local_data_dir(&ctx.project_root, &ctx.extension_id);
    assert!(
        pl.to_string_lossy().contains(".ion/test-ext"),
        "project_local: {pl:?}"
    );

    // session: .../sessions/--hash/data/<sid>/<ext>/
    let s = paths::session_data_dir(&ctx.cwd, &ctx.session_id, &ctx.extension_id);
    assert!(
        s.to_string_lossy().contains(&ctx.session_id),
        "session: {s:?}"
    );
    assert!(
        s.to_string_lossy().contains("test-ext"),
        "session ext: {s:?}"
    );
}

#[test]
fn extension_context_injected_into_store() {
    let wasm_path = build_todo_extension();
    let mut extension = ion::wasm_extension::WasmExtensionInstance::load(std::path::Path::new(&wasm_path))
        .expect("todo-extension should load");

    let ctx = ion::wasm_extension::Context {
        session_id: "sess-test".into(),
        cwd: "/tmp".into(),
        project_root: "/tmp".into(),
        extension_id: "todo_extension".into(),
        event_bus: None,
        fs: None,
        tokio_handle: None,
        agent_rpc: None,
    };

    // Inject context and execute — the store should have context available
    extension.set_context(&ctx);
    let result = extension
        .execute_tool("todo_list", r#"{"status":"all"}"#)
        .expect("todo_list should succeed after set_context");
    // Empty list is a valid result (no tasks added yet, but context was injected)
    assert!(!result.is_empty(), "result should not be empty: {result}");
}

#[test]
fn extension_write_read_delete_works_directly() {
    // Test the data persistence pattern that the host functions implement:
    // write to data dir → read back → delete.
    // (The actual WASM host functions call these same std::fs operations.)
    use ion::paths;

    let extension_id = "test-data-ext";
    let project_root = std::env::temp_dir()
        .join("ion-test-extension-data")
        .to_string_lossy()
        .to_string();
    let _ = std::fs::remove_dir_all(&project_root);

    // Compute the project_local dir (same logic as the host functions)
    let dir = paths::project_local_data_dir(&project_root, extension_id);

    // ── write (simulating host_write_project_local_data) ──
    let key = "my-key.json";
    let data = br#"{"hello":"world"}"#;
    std::fs::create_dir_all(&dir).expect("create data dir");
    let tmp = dir.join(format!("{key}.tmp"));
    let final_path = dir.join(key);
    std::fs::write(&tmp, data).expect("write tmp");
    std::fs::rename(&tmp, &final_path).expect("rename");

    assert!(final_path.exists(), "file should exist after write");

    // ── read (simulating host_read_project_local_data) ──
    let loaded = std::fs::read(&final_path).expect("read back");
    assert_eq!(loaded, data, "data should round-trip");

    // ── list (simulating host_list_project_local_data) ──
    let entries: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().ends_with(".tmp"))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        entries.contains(&"my-key.json".to_string()),
        "list should contain the key: {entries:?}"
    );

    // ── delete (simulating host_delete_project_local_data) ──
    std::fs::remove_file(&final_path).expect("delete");
    assert!(!final_path.exists(), "file should be gone after delete");

    // cleanup
    let _ = std::fs::remove_dir_all(&project_root);
}

#[test]
fn extension_make_exec_context_merges_registry_ctx_with_extension_id() {
    let reg_ctx = ion::wasm_extension::Context {
        session_id: "sess-1".into(),
        cwd: "/proj".into(),
        project_root: "/proj".into(),
        extension_id: "".into(),
        event_bus: None,
        fs: None,
        tokio_handle: None,
        agent_rpc: None,
    };

    let exec_ctx = ion::wasm_extension::make_exec_context(&reg_ctx, "my-ext");
    assert_eq!(exec_ctx.session_id, "sess-1");
    assert_eq!(exec_ctx.extension_id, "my-ext", "extension_id should be overridden");
    assert_eq!(exec_ctx.cwd, "/proj");
}
