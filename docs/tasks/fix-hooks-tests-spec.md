# Task Spec: 修复 2 个 hooks 失败测试

> **状态：待 B 执行** | 改动范围：仅 `src/hooks/extension.rs` 的 `#[cfg(test)]` 模块 | 不碰产品代码

## 背景

`cargo test --lib` 有 2 个失败：

```
hooks::extension::tests::test_has_hooks_returns_false_for_nonexistent_dir
hooks::extension::tests::test_new_preserves_project_dir_usable_for_has_hooks
```

## 根因

`has_hooks(project_dir)` 的**真实语义**是：合并全局 `~/.ion/hooks.json` + `<project_dir>/.ion/hooks.json`，任一非空返回 true。见 `src/hooks/mod.rs:152` `load_fresh()`。

测试假设 `has_hooks(不存在的 dir)` = false，但如果运行测试的机器上全局 `~/.ion/hooks.json` 有内容（比如开发机的 `~/.ion/hooks.json` 配了 SessionStart hook），`has_hooks` 永远返回 true，断言失败。

**这是测试逻辑错误，不是产品 bug。** `has_hooks` 的合并语义是正确的（产品需要全局+项目级合并）。

## 修法（B 执行）

在 `src/hooks/extension.rs` 的 `#[cfg(test)]` 模块里，改这 2 个测试，让它们在隔离的临时 HOME 环境下运行，避免全局 `~/.ion/hooks.json` 污染。

### 关键信息

`crate::paths::root()` 读 `HOME` 环境变量（`src/paths.rs:77-82`）：
```rust
pub fn root() -> PathBuf {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(ION_DIR)
}
```

所以测试只要临时设置 `HOME` 到一个空临时目录，`paths::root()` 就指向 `<tmp>/.ion`（无 hooks.json），`has_hooks` 返回 false。

### 测试 1：test_has_hooks_returns_false_for_nonexistent_dir

**当前代码**（`src/hooks/extension.rs:582-587`）：
```rust
#[test]
fn test_has_hooks_returns_false_for_nonexistent_dir() {
    let dir = PathBuf::from("/tmp/ion-test-definitely-nonexistent-12345");
    assert!(!HookExtension::has_hooks(&dir));
}
```

**改为**：用 `std::env::set_var("HOME", tmp_dir)` 隔离全局 config，测试完后恢复原 HOME。用 `tempfile::TempDir` 创建临时 HOME（项目已依赖 tempfile）。

```rust
#[test]
fn test_has_hooks_returns_false_for_nonexistent_dir() {
    // Isolate from the global ~/.ion/hooks.json by pointing HOME at an empty temp dir.
    // has_hooks() merges global + project config; without isolation this test fails
    // on dev machines that have ~/.ion/hooks.json configured.
    let tmp_home = tempfile::TempDir::new().expect("create temp HOME");
    let orig_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", tmp_home.path());
    let dir = PathBuf::from("/tmp/ion-test-definitely-nonexistent-12345");
    assert!(!HookExtension::has_hooks(&dir));
    // Restore HOME
    match orig_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}
```

### 测试 2：test_new_preserves_project_dir_usable_for_has_hooks

**当前代码**（`src/hooks/extension.rs:616-626`）：
```rust
#[test]
fn test_new_preserves_project_dir_usable_for_has_hooks() {
    let dir = PathBuf::from("/tmp/ion-ext-test-no-hooks-98765");
    let ext = HookExtension::new(dir.clone(), None, None, None, None, None);
    assert_eq!(ext.name(), "hooks");
    assert!(!HookExtension::has_hooks(&dir));
}
```

**改为**：同样的 HOME 隔离模式：
```rust
#[test]
fn test_new_preserves_project_dir_usable_for_has_hooks() {
    let tmp_home = tempfile::TempDir::new().expect("create temp HOME");
    let orig_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", tmp_home.path());
    let dir = PathBuf::from("/tmp/ion-ext-test-no-hooks-98765");
    let ext = HookExtension::new(dir.clone(), None, None, None, None, None);
    assert_eq!(ext.name(), "hooks");
    assert!(!HookExtension::has_hooks(&dir));
    match orig_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}
```

### 注意事项

1. **set_var 在多线程测试里有 race condition 风险**。但 cargo test 默认多线程跑，这 2 个测试同时 set_var/restore HOME 可能互相干扰。**解决方案**：给这 2 个测试加 `#[serial]`（需要 `serial_test` crate）或者用更安全的方式。

   **推荐方案（不用加 crate 依赖）**：把 HOME 隔离逻辑抽成一个 helper 函数，2 个测试都调用它，但由于线程问题，**最干净的做法是让这 2 个测试不依赖全局状态**——直接测试 `load_fresh` 在隔离环境的行为，或者接受这 2 个测试是"smoke test"只断言返回 bool（像已有的 `test_has_hooks_false_for_empty_temp_dir` 那样）。

   **B 的判断权**：B 可以选择 (a) 用 set_var + 接受潜在 flaky，(b) 改成 smoke test 只断言 bool，(c) 重构成不依赖 HOME 的纯函数测试。选最稳健的方案。

2. **不要改 `has_hooks()` 或 `load_fresh()` 的产品逻辑**——它们的合并语义是对的。

3. **不要改测试名或删测试**——保留测试覆盖。

## 验证

B 改完后跑：
```bash
cargo test --lib hooks::extension::tests
```
应该全部通过（包括这 2 个修好的 + 其他原有测试）。

然后跑全量：
```bash
cargo test --lib 2>&1 | tail -5
```
应该是 `1013 passed; 0 failed`（修了 2 个，数量不变因为没加没减测试）。

## 守门

- ✅ `cargo test --lib` 全过（0 failed）
- ✅ 无 U+FFFD（测试代码用英文 comment）
- ✅ 不碰 `src/hooks/mod.rs`（产品逻辑）
- ✅ 不加新 crate 依赖（除非 B 选 serial_test 方案且 Cargo.toml 守门放行——但推荐不加）
