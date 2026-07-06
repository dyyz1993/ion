//! Integration tests for Todo + Plan WASM plugins and PlanExtension.
//!
//! Verifies:
//! - WASM plugins load and register tools correctly
//! - WASM tool execution returns expected results
//! - PlanExtension correctly restricts tools in plan mode
//! - PlanExtension on_system_prompt injects planning instructions

use ion::agent::extension::Extension;

// ---------------------------------------------------------------------------
// Helpers: build WASM plugins
// ---------------------------------------------------------------------------

fn build_todo_plugin() -> String {
    build_wasm_plugin("todo-plugin", "todo_plugin.wasm")
}

fn build_plan_plugin() -> String {
    build_wasm_plugin("plan-plugin", "plan_plugin.wasm")
}

fn build_wasm_plugin(pkg_dir: &str, wasm_file: &str) -> String {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let pkg_path = manifest_dir.join(pkg_dir);

    // Build the WASM plugin
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
        .expect(&format!("failed to build {pkg_dir}"));

    assert!(
        output.status.success(),
        "{pkg_dir} build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Path to the compiled WASM binary
    let wasm_path = pkg_path
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join(wasm_file);

    assert!(
        wasm_path.exists(),
        "WASM file not found at {}",
        wasm_path.display()
    );

    wasm_path.to_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// Todo plugin tests
// ---------------------------------------------------------------------------

#[test]
fn todo_plugin_loads_and_registers_tools() {
    let wasm_path = build_todo_plugin();
    let plugin = ion::wasm_extension::Extension::load(std::path::Path::new(&wasm_path))
        .expect("todo-plugin should load");

    let names: Vec<&str> = plugin.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"todo_create"), "should register todo_create");
    assert!(names.contains(&"todo_update"), "should register todo_update");
    assert!(names.contains(&"todo_list"), "should register todo_list");
    assert_eq!(plugin.tools.len(), 3, "should register exactly 3 tools");
}

#[test]
fn todo_plugin_create_and_list() {
    let wasm_path = build_todo_plugin();
    let mut plugin = ion::wasm_extension::Extension::load(std::path::Path::new(&wasm_path))
        .expect("todo-plugin should load");

    // Create a todo list
    let result = plugin
        .execute_tool("todo_create", r#"{"items":[{"id":"1","content":"调研"},{"id":"2","content":"实现"}]}"#)
        .expect("todo_create should succeed");
    assert!(result.contains(r#""status":"ok""#), "result should be ok: {result}");
    assert!(result.contains(r#""count":2"#), "should report 2 items: {result}");

    // List todos
    let list = plugin
        .execute_tool("todo_list", "{}")
        .expect("todo_list should succeed");
    assert!(list.contains(r#""id":"1""#), "should contain id 1: {list}");
    assert!(list.contains(r#""status":"pending""#), "items should be pending: {list}");
}

#[test]
fn todo_plugin_update_status() {
    let wasm_path = build_todo_plugin();
    let mut plugin = ion::wasm_extension::Extension::load(std::path::Path::new(&wasm_path))
        .expect("todo-plugin should load");

    // Create
    plugin
        .execute_tool("todo_create", r#"{"items":[{"id":"1","content":"调研"}]}"#)
        .unwrap();

    // Update to in_progress
    let result = plugin
        .execute_tool("todo_update", r#"{"id":"1","status":"in_progress"}"#)
        .expect("todo_update should succeed");
    assert!(result.contains(r#""status":"in_progress""#), "should be in_progress: {result}");

    // Update to completed
    let result2 = plugin
        .execute_tool("todo_update", r#"{"id":"1","status":"completed"}"#)
        .expect("todo_update should succeed");
    assert!(result2.contains(r#""status":"completed""#), "should be completed: {result2}");

    // Update with details
    let result3 = plugin
        .execute_tool("todo_update", r#"{"id":"1","status":"failed","details":"出错了"}"#)
        .expect("todo_update should succeed");
    assert!(result3.contains(r#""details":"出错了""#), "should have details: {result3}");
}

#[test]
fn todo_plugin_nonexistent_item() {
    let wasm_path = build_todo_plugin();
    let mut plugin = ion::wasm_extension::Extension::load(std::path::Path::new(&wasm_path))
        .expect("todo-plugin should load");

    plugin
        .execute_tool("todo_create", r#"{"items":[{"id":"1","content":"调研"}]}"#)
        .unwrap();

    let result = plugin
        .execute_tool("todo_update", r#"{"id":"999","status":"completed"}"#)
        .expect("should return error gracefully");
    assert!(result.contains(r#""error""#), "should report error: {result}");
}

// ---------------------------------------------------------------------------
// Plan plugin tests
// ---------------------------------------------------------------------------

#[test]
fn plan_plugin_loads_and_registers_tools() {
    let wasm_path = build_plan_plugin();
    let plugin = ion::wasm_extension::Extension::load(std::path::Path::new(&wasm_path))
        .expect("plan-plugin should load");

    let names: Vec<&str> = plugin.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"plan_enter"), "should register plan_enter");
    assert!(names.contains(&"plan_exit"), "should register plan_exit");
    assert_eq!(plugin.tools.len(), 2, "should register exactly 2 tools");
}

#[test]
fn plan_plugin_enter_and_exit() {
    let wasm_path = build_plan_plugin();
    let mut plugin = ion::wasm_extension::Extension::load(std::path::Path::new(&wasm_path))
        .expect("plan-plugin should load");

    let enter = plugin
        .execute_tool("plan_enter", r#"{"plan_path":"/tmp/test-plan.md"}"#)
        .expect("plan_enter should succeed");
    assert!(enter.contains(r#""mode":"plan""#), "should return plan mode: {enter}");

    let exit = plugin
        .execute_tool("plan_exit", "{}")
        .expect("plan_exit should succeed");
    assert!(exit.contains(r#""mode":"normal""#), "should return normal mode: {exit}");
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
        .before_tool_call(&make_tool_call("bash", r#"{"command":"ls"}"#))
        .await;
    assert!(result.is_ok(), "bash should be allowed in normal mode");

    let result = ext
        .before_tool_call(&make_tool_call("write", r#"{"path":"/tmp/x"}"#))
        .await;
    assert!(result.is_ok(), "write should be allowed in normal mode");
}

#[tokio::test]
async fn plan_extension_plan_mode_restricts_tools() {
    let ext = ion::agent::plan_extension::PlanExtension::new();

    // Enter plan mode via after_tool_call
    ext.after_tool_call(
        &make_tool_call("plan_enter", r#"{"plan_path":"/tmp/plan.md"}"#),
        &make_tool_result(),
    )
    .await
    .unwrap();
    assert!(ext.is_plan_mode(), "should be in plan mode");

    // Plan-allowed tools should still work
    let result = ext
        .before_tool_call(&make_tool_call("read", r#"{"file_path":"/tmp/x"}"#))
        .await;
    assert!(result.is_ok(), "read should be allowed in plan mode");

    let result = ext
        .before_tool_call(&make_tool_call("plan_exit", "{}"))
        .await;
    assert!(result.is_ok(), "plan_exit should be allowed in plan mode");

    // Non-plan tools should be rejected
    let result = ext
        .before_tool_call(&make_tool_call("calculator", r#"{"expression":"1+1"}"#))
        .await;
    assert!(result.is_err(), "calculator should be rejected in plan mode");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not available in plan mode"), "error should mention plan mode: {err}");
}

#[tokio::test]
async fn plan_extension_exit_plan_mode_restores_tools() {
    let ext = ion::agent::plan_extension::PlanExtension::new();

    // Enter plan mode
    ext.after_tool_call(
        &make_tool_call("plan_enter", r#"{"plan_path":"/tmp/p"}"#),
        &make_tool_result(),
    )
    .await
    .unwrap();

    // Exit plan mode
    ext.after_tool_call(
        &make_tool_call("plan_exit", "{}"),
        &make_tool_result(),
    )
    .await
    .unwrap();
    assert!(!ext.is_plan_mode(), "should exit plan mode");

    // calculator should be allowed again
    let result = ext
        .before_tool_call(&make_tool_call("calculator", r#"{"expression":"1+1"}"#))
        .await;
    assert!(result.is_ok(), "calculator should be allowed after plan_exit");
}

#[tokio::test]
async fn plan_extension_injects_system_prompt_in_plan_mode() {
    let ext = ion::agent::plan_extension::PlanExtension::new();

    // Normal mode: should not inject
    let mut prompt = "base prompt".to_string();
    ext.on_system_prompt(&mut prompt).await.unwrap();
    assert_eq!(prompt, "base prompt", "should not modify prompt in normal mode");

    // Enter plan mode
    ext.after_tool_call(
        &make_tool_call("plan_enter", r#"{"plan_path":"/tmp/my-plan.md"}"#),
        &make_tool_result(),
    )
    .await
    .unwrap();

    // Plan mode: should inject instructions
    let mut prompt2 = "base prompt".to_string();
    ext.on_system_prompt(&mut prompt2).await.unwrap();
    assert!(prompt2.contains("PLAN MODE"), "should inject PLAN MODE marker");
    assert!(prompt2.contains("/tmp/my-plan.md"), "should inject plan path");
    assert!(prompt2.contains("plan_exit"), "should mention plan_exit");
    assert!(prompt2.contains("Available tools"), "should list available tools");
}

#[tokio::test]
async fn plan_extension_tracks_plan_path() {
    let ext = ion::agent::plan_extension::PlanExtension::new();

    ext.after_tool_call(
        &make_tool_call("plan_enter", r#"{"plan_path":"/tmp/custom-plan.md"}"#),
        &make_tool_result(),
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

// ── Edge case tests ──────────────────────────────────────────────────────────

#[test]
fn todo_plugin_edge_empty_array() {
    let mut p = load_todo();
    let r = p.execute_tool("todo_create", r#"{"items":[]}"#).expect("should succeed");
    assert!(r.contains(r#""count":0"#), "zero items: {r}");
    let list = p.execute_tool("todo_list", "{}").expect("list");
    assert!(list.contains(r#""items":[]"#), "empty list: {list}");
}

#[test]
fn todo_plugin_edge_invalid_status() {
    let mut p = load_todo();
    p.execute_tool("todo_create", r#"{"items":[{"id":"1","content":"x"}]}"#).unwrap();
    let r = p.execute_tool("todo_update", r#"{"id":"1","status":"invalid"}"#).expect("should error");
    assert!(r.contains(r#""error""#), "invalid status should error: {r}");
}

#[test]
fn todo_plugin_edge_update_empty_list() {
    let mut p = load_todo();
    let r = p.execute_tool("todo_update", r#"{"id":"1","status":"completed"}"#).expect("should error");
    assert!(r.contains(r#""error""#), "update empty list should error: {r}");
}

#[test]
fn todo_plugin_edge_special_chars() {
    let mut p = load_todo();
    let r = p.execute_tool("todo_create",
        r#"{"items":[{"id":"1","content":"hello \"world\" & <test>"}]}"#)
        .expect("should succeed");
    assert!(r.contains("hello"), "special chars: {r}");
    let list = p.execute_tool("todo_list", "{}").expect("list");
    assert!(list.contains("hello"), "list should contain special chars: {list}");
}

#[test]
fn todo_plugin_edge_large_list() {
    let mut p = load_todo();
    let items: String = (0..20).map(|i| {
        format!(r#"{{"id":"{}","content":"item {}"}}"#, i, i)
    }).collect::<Vec<_>>().join(",");
    let r = p.execute_tool("todo_create", &format!(r#"{{"items":[{}]}}"#, items))
        .expect("should handle 20 items");
    assert!(r.contains(r#""count":20"#), "20 items: {r}");
}

#[tokio::test]
async fn plan_extension_double_enter_is_idempotent() {
    let ext = ion::agent::plan_extension::PlanExtension::new();
    ext.after_tool_call(&make_tool_call("plan_enter", r#"{"plan_path":"/tmp/a"}"#), &make_tool_result()).await.unwrap();
    ext.after_tool_call(&make_tool_call("plan_enter", r#"{"plan_path":"/tmp/b"}"#), &make_tool_result()).await.unwrap();
    assert!(ext.is_plan_mode(), "still in plan mode");
    let mut prompt = String::new();
    ext.on_system_prompt(&mut prompt).await.unwrap();
    assert!(prompt.contains("/tmp/b"), "should use latest path: {prompt}");
}

/// Helper: load the todo plugin once (reused by edge tests).
fn load_todo() -> ion::wasm_extension::Extension {
    let wasm_path = build_todo_plugin();
    ion::wasm_extension::Extension::load(std::path::Path::new(&wasm_path))
        .expect("todo-plugin should load")
}

// ---------------------------------------------------------------------------
// Registry — hot‑pluggable WASM plugin lifecycle  (P1–P4)
// ---------------------------------------------------------------------------

#[test]
fn plugin_registry_add_list_remove() {
    let wasm_path = build_todo_plugin();
    let registry = ion::wasm_extension::Registry::new();

    // P1: add → should return tool defs
    let tool_defs = registry.add(&wasm_path)
        .expect("plugin_registry::add should load todo-plugin");
    let names: Vec<&str> = tool_defs.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"todo_create"), "add should register todo_create");
    assert!(names.contains(&"todo_list"),   "add should register todo_list");
    assert!(names.contains(&"todo_update"), "add should register todo_update");
    assert_eq!(tool_defs.len(), 3, "exactly 3 tools from todo-plugin");

    // P2: list → should include the loaded plugin
    let plugins = registry.list();
    assert_eq!(plugins.len(), 1, "list should contain the loaded plugin");
    let p = &plugins[0];
    assert!(p.path.ends_with("todo_plugin.wasm"), "path should end with .wasm");
    assert_eq!(p.version, 1, "todo-plugin version should be 1");
    assert_eq!(p.tools.len(), 3, "plugin info should list 3 tools");

    // P3: remove → should return tool names and clear from list
    let removed = registry.remove(&wasm_path)
        .expect("plugin_registry::remove should succeed");
    assert_eq!(removed.len(), 3, "remove should return 3 tool names");
    assert!(removed.contains(&"todo_create".to_string()));

    let empty = registry.list();
    assert_eq!(empty.len(), 0, "after remove, list should be empty");

    // P4: re‑add after removal works
    let tool_defs2 = registry.add(&wasm_path)
        .expect("re‑add after remove should work");
    assert_eq!(tool_defs2.len(), 3, "re‑added plugin should register tools");
    assert_eq!(registry.list().len(), 1, "re‑added plugin should show in list");
}

#[test]
fn plugin_registry_reload_replaces_instance() {
    let wasm_path = build_todo_plugin();
    let registry = ion::wasm_extension::Registry::new();

    // Load once
    registry.add(&wasm_path).expect("initial load");
    let plugins_before = registry.list();
    assert_eq!(plugins_before.len(), 1);
    let version_before = plugins_before[0].version;
    let tools_before = plugins_before[0].tools.clone();

    // Reload (same .wasm file, fresh instance)
    let tool_defs = registry.reload(&wasm_path)
        .expect("reload should succeed");
    assert_eq!(tool_defs.len(), 3, "reload should register the same tools");

    // The entry should be replaced
    let plugins_after = registry.list();
    assert_eq!(plugins_after.len(), 1, "still exactly one plugin after reload");
    // Version should match (same .wasm file)
    assert_eq!(plugins_after[0].version, version_before, "version unchanged after reload");
    assert_eq!(plugins_after[0].tools, tools_before, "tools unchanged after reload");
}

#[test]
fn plugin_registry_add_same_path_twice_is_reload() {
    let wasm_path = build_todo_plugin();
    let registry = ion::wasm_extension::Registry::new();

    // add twice → second call replaces the first (reload semantics)
    registry.add(&wasm_path).expect("first add");
    registry.add(&wasm_path).expect("second add (replaces)");

    // list should still have exactly 1 entry
    let plugins = registry.list();
    assert_eq!(plugins.len(), 1, "second add should replace, not duplicate");
}

#[test]
fn plugin_registry_remove_nonexistent_returns_error() {
    let registry = ion::wasm_extension::Registry::new();
    let result = registry.remove("/nonexistent/path.wasm");
    assert!(result.is_err(), "remove of nonexistent path should fail");
}

#[test]
fn plugin_registry_can_hold_multiple_plugins() {
    let todo_path = build_todo_plugin();
    let plan_path = build_plan_plugin();
    let registry = ion::wasm_extension::Registry::new();

    registry.add(&todo_path).expect("load todo");
    registry.add(&plan_path).expect("load plan");

    let plugins = registry.list();
    assert_eq!(plugins.len(), 2, "should hold 2 plugins");

    // Each has its own tools
    let todo_info = plugins.iter().find(|p| p.tools.contains(&"todo_list".to_string())).unwrap();
    let plan_info = plugins.iter().find(|p| p.tools.contains(&"plan_enter".to_string())).unwrap();
    assert!(todo_info.path.contains("todo_plugin"), "todo path");
    assert!(plan_info.path.contains("plan_plugin"), "plan path");

    // Remove one, the other remains
    registry.remove(&plan_path).expect("remove plan");
    assert_eq!(registry.list().len(), 1, "only todo remains");
    assert_eq!(registry.list()[0].tools.len(), 3, "todo still has 3 tools");
}

// ---------------------------------------------------------------------------
// Plugin data dimensions — paths, context injection, ext_name derivation
// ---------------------------------------------------------------------------

#[test]
fn plugin_ext_name_from_path() {
    // file stem wins
    assert_eq!(
        ion::wasm_extension::ext_name_from_path("/home/user/todo-plugin/target/release/todo_plugin.wasm"),
        "todo_plugin",
    );
    assert_eq!(
        ion::wasm_extension::ext_name_from_path("/tmp/my_plugin.wasm"),
        "my_plugin",
    );
}

#[test]
fn plugin_data_dimension_paths_are_correct() {
    use ion::paths;

    let ctx = ion::wasm_extension::Context {
        session_id: "sess-abc".into(),
        cwd: "/tmp/work".into(),
        project_root: "/tmp/work".into(),
        ext_name: "test-ext".into(),
    };

    // global: ~/.ion/agent/extensions-data/<ext>/
    let g = paths::global_data_dir(&ctx.ext_name);
    assert!(
        g.to_string_lossy().contains("extensions-data/test-ext"),
        "global: {g:?}"
    );

    // project (in ~/.ion): ~/.ion/agent/project-data/<enc>/<ext>/
    let p = paths::project_data_dir(&ctx.project_root, &ctx.ext_name);
    assert!(
        p.to_string_lossy().contains("project-data/"),
        "project: {p:?}"
    );
    assert!(
        p.to_string_lossy().contains("test-ext"),
        "project ext: {p:?}"
    );

    // project_local (in project directory): <root>/.ion/<ext>/
    let pl = paths::project_local_data_dir(&ctx.project_root, &ctx.ext_name);
    assert!(
        pl.to_string_lossy().contains(".ion/test-ext"),
        "project_local: {pl:?}"
    );

    // session: .../sessions/--hash/data/<sid>/<ext>/
    let s = paths::session_data_dir(&ctx.cwd, &ctx.session_id, &ctx.ext_name);
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
fn plugin_context_injected_into_store() {
    let wasm_path = build_todo_plugin();
    let mut plugin = ion::wasm_extension::Extension::load(std::path::Path::new(&wasm_path))
        .expect("todo-plugin should load");

    let ctx = ion::wasm_extension::Context {
        session_id: "sess-test".into(),
        cwd: "/tmp".into(),
        project_root: "/tmp".into(),
        ext_name: "todo-plugin".into(),
    };

    // Inject context and execute — the store should have context available
    plugin.set_context(&ctx);
    let result = plugin
        .execute_tool("todo_list", "{}")
        .expect("todo_list should succeed after set_context");
    assert!(
        result.contains(r#""status":"ok""#) || result.contains("items"),
        "result should be valid: {result}"
    );
}

#[test]
fn plugin_write_read_delete_works_directly() {
    // Test the data persistence pattern that the host functions implement:
    // write to data dir → read back → delete.
    // (The actual WASM host functions call these same std::fs operations.)
    use ion::paths;

    let ext_name = "test-data-ext";
    let project_root = std::env::temp_dir()
        .join("ion-test-plugin-data")
        .to_string_lossy()
        .to_string();
    let _ = std::fs::remove_dir_all(&project_root);

    // Compute the project_local dir (same logic as the host functions)
    let dir = paths::project_local_data_dir(&project_root, ext_name);

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
    assert!(entries.contains(&"my-key.json".to_string()), "list should contain the key: {entries:?}");

    // ── delete (simulating host_delete_project_local_data) ──
    std::fs::remove_file(&final_path).expect("delete");
    assert!(!final_path.exists(), "file should be gone after delete");

    // cleanup
    let _ = std::fs::remove_dir_all(&project_root);
}

#[test]
fn plugin_make_exec_context_merges_registry_ctx_with_ext_name() {
    let reg_ctx = ion::wasm_extension::Context {
        session_id: "sess-1".into(),
        cwd: "/proj".into(),
        project_root: "/proj".into(),
        ext_name: "".into(),
    };

    let exec_ctx = ion::wasm_extension::make_exec_context(&reg_ctx, "my-ext");
    assert_eq!(exec_ctx.session_id, "sess-1");
    assert_eq!(exec_ctx.ext_name, "my-ext", "ext_name should be overridden");
    assert_eq!(exec_ctx.cwd, "/proj");
}
