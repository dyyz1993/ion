//! 受保护路径集 — 安全对抗评审 P0（内核级写保护，不可绕过）。
//!
//! 防御目标（红蓝对抗评审 2026-09-10）：prompt injection 诱导 agent 改写
//! `~/.ion/config.json` 的 base_url → 此后所有会话的 LLM 流量（含 key 请求头）
//! 持久流经攻击者收集器（config base_url 持久 MITM）。同类向量：auth.json
//! （key 泄露进上下文）、hooks.json（命令执行供应链）、settings/path-permissions
//! （权限体系自我改写）。
//!
//! 设计要点：
//! - 检查位于 SecuredRuntime 的权限引擎**之前**、无条件执行——deny 永不被
//!   stored-decision/allow 遮蔽（对齐安全评审 P0#2 语义）
//! - canonicalize 后比对（内核解析真实路径，`../` 与符号链接不绕过）
//! - 默认集不可关闭（未上线项目无需兼容开关）；`runtime.protected_paths_extra`
//!   只允许**追加**
//! - 合法修改走 agent 之外：`ion config set` / 用户手动编辑（直接 fs，不过本层）

use std::path::{Path, PathBuf};

/// 默认受保护路径（~/.ion 下的核心配置/凭据/权限文件）。
pub fn default_protected(root: &Path) -> Vec<PathBuf> {
    [
        "config.json",
        "auth.json",
        "hooks.json",
        "settings.json",
        "path-permissions.json",
        "agent/models.json",
    ]
    .iter()
    .map(|f| root.join(f))
    .collect()
}

/// 规范化一个可能不存在的路径：文件存在 → canonicalize；
/// 不存在 → canonicalize 父目录 + 文件名（新建文件场景）。
/// 两层都解析失败 → None（调用方视为不受保护——连父目录都不存在的
/// 路径不可能是受保护配置文件）。
/// 词法规范化：逐组件解析 `.` 和 `..`（不跟随符号链接）。
/// 必须先做这一步——canonicalize 对"未存在目录下的 .."解析结果不可靠
///（实测 macOS 上 ../.. 深层会被错误折叠）。
fn normalize_lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut comps: Vec<std::ffi::OsString> = Vec::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                comps.pop();
            }
            Component::CurDir => {}
            other => comps.push(other.as_os_str().to_os_string()),
        }
    }
    comps.iter().collect()
}

fn canonicalize_loose(p: &Path) -> Option<PathBuf> {
    let p = normalize_lexical(p);
    if p.exists() {
        return std::fs::canonicalize(&p).ok();
    }
    let parent = p.parent()?;
    let name = p.file_name()?;
    let canon_parent = std::fs::canonicalize(parent).ok()?;
    Some(canon_parent.join(name))
}

/// 判断 path 是否命中受保护集（extras 为用户追加项，同样 canonicalize 比对）。
pub fn is_protected(root: &Path, extras: &[String], path: &str) -> bool {
    let Some(canon) = canonicalize_loose(Path::new(path)) else {
        return false;
    };
    let mut all = default_protected(root);
    for e in extras {
        all.push(PathBuf::from(e.as_str()));
    }
    all.iter()
        .any(|p| canonicalize_loose(p) == Some(canon.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_protected_basic_and_traversal() {
        let root = std::env::temp_dir().join(format!("pp-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("agent")).unwrap();
        std::fs::write(root.join("config.json"), "{}").unwrap();

        // 直接命中
        assert!(is_protected(
            &root,
            &[],
            &root.join("config.json").to_string_lossy()
        ));
        // 路径穿越形式（../ 绕行回受保护目录）→ 词法规范化 + canonicalize 后仍命中
        let traversal = format!("{}/agent/../config.json", root.display());
        assert!(
            is_protected(&root, &[], &traversal),
            "traversal must be resolved before matching"
        );
        // 越出受保护目录的 .. → 不是受保护文件（语义正确性）
        let outside = format!("{}/../other-root/config.json", root.display());
        assert!(!is_protected(&root, &[], &outside));
        // 不存在的兄弟文件不受保护
        assert!(!is_protected(
            &root,
            &[],
            &root.join("other.json").to_string_lossy()
        ));
        // extras 生效
        let extra = root.join("custom-important.json");
        std::fs::write(&extra, "{}").unwrap();
        assert!(!is_protected(&root, &[], &extra.to_string_lossy()));
        assert!(is_protected(
            &root,
            &[extra.to_string_lossy().to_string()],
            &extra.to_string_lossy()
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_nonexistent_outside_root_not_protected() {
        let root = std::env::temp_dir().join("pp-nonexistent-root");
        assert!(!is_protected(&root, &[], "/definitely/not/here.json"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
