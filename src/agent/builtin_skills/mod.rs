//! 内置 skill（编译进二进制，不可移除层）。
//!
//! 与文件系统 skill（~/.ion/agent/skills/、<project>/.ion/skills/）的区别：
//! - 内容 include_str! 进二进制——文件删了也在，config skills.disabled 也禁不掉
//! - 正文默认注入 system prompt（每个会话/worker 自带，无需 LLM 主动 inject）
//! - 同时注册进 SkillTool 的 list（LLM 可随时 skill inject 重新取用）
//!
//! 新增内置 skill：把 .md 放本目录，加进 BUILTIN_SKILLS 数组，补一个单测。

/// 内置 skill：内容编译进二进制（不可禁用/不可删除）。
pub struct BuiltinSkill {
    pub name: &'static str,
    pub content: &'static str,
}

pub const BUILTIN_SKILLS: &[BuiltinSkill] = &[
    BuiltinSkill {
        name: "create-hooks",
        content: include_str!("create-hooks.md"),
    },
    BuiltinSkill {
        name: "create-subagent",
        content: include_str!("create-subagent.md"),
    },
];

/// 内置 skill 正文拼进 system prompt 的段落（"默认加载"的核心）。
/// 每个会话/worker 启动时注入一次；正在运行的会话不热更新（下个会话生效）。
pub fn builtin_skills_prompt() -> String {
    let mut out = String::from(
        "# ION 内置技能（始终可用，无法移除）\n\
         以下技能内容已默认加载。涉及创建 hooks 或子智能体时，严格按其步骤与生效时机行事。\n\n",
    );
    for s in BUILTIN_SKILLS {
        out.push_str(&format!("---\n{}\n---\n\n", s.content));
    }
    out
}

/// 是否内置 skill 名（disabled 黑名单豁免判定用）。
pub fn is_builtin_skill(name: &str) -> bool {
    BUILTIN_SKILLS.iter().any(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_skills_present() {
        assert!(BUILTIN_SKILLS.len() >= 2);
        for s in BUILTIN_SKILLS {
            assert!(!s.content.is_empty(), "{} 内容为空", s.name);
            assert!(
                s.content.contains("生效时机"),
                "{} 必须写明生效时机（用户硬性要求）",
                s.name
            );
        }
    }

    #[test]
    fn builtin_prompt_contains_both() {
        let p = builtin_skills_prompt();
        assert!(p.contains("create-hooks"));
        assert!(p.contains("create-subagent"));
        assert!(p.contains("hooks.json"));
    }

    #[test]
    fn is_builtin_recognizes() {
        assert!(is_builtin_skill("create-hooks"));
        assert!(is_builtin_skill("create-subagent"));
        assert!(!is_builtin_skill("random-skill"));
    }
}
