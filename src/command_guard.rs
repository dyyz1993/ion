use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel { High, Medium, Low }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskPattern {
    pub pattern: String,
    pub message: String,
    pub level: RiskLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Clone, Debug)]
pub enum GuardDecision { Allow, Ask(RiskPattern), Deny(RiskPattern) }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandGuard {
    #[serde(default)] pub whitelist: Vec<String>,
    #[serde(default)] pub risk_patterns: Vec<RiskPattern>,
}

impl Default for CommandGuard {
    fn default() -> Self {
        Self { whitelist: default_whitelist(), risk_patterns: default_risk_patterns() }
    }
}

impl CommandGuard {
    pub fn new(whitelist: Vec<String>, risk_patterns: Vec<RiskPattern>) -> Self {
        Self { whitelist, risk_patterns }
    }

    pub fn check(&self, command: &str) -> GuardDecision {
        let t = command.trim();
        if t.is_empty() { return GuardDecision::Allow; }
        let lower = t.to_lowercase();

        // 风险模式优先（白名单不覆盖风险）
        for p in &self.risk_patterns {
            if contains(&lower, &p.pattern.to_lowercase()) {
                return match p.level {
                    RiskLevel::High => GuardDecision::Deny(p.clone()),
                    RiskLevel::Medium => GuardDecision::Ask(p.clone()),
                    RiskLevel::Low => GuardDecision::Allow,
                };
            }
        }

        // 白名单（安全命令快速放行）
        for prefix in &self.whitelist {
            if t == prefix.as_str() || t.starts_with(&format!("{} ", prefix)) {
                return GuardDecision::Allow;
            }
        }
        GuardDecision::Allow
    }

    pub fn add_whitelist(&mut self, prefix: &str) {
        let p = prefix.trim().to_string();
        if !self.whitelist.contains(&p) { self.whitelist.push(p); }
    }
}

fn contains(cmd: &str, pat: &str) -> bool {
    if pat.ends_with(' ') {
        let t = pat.trim_end();
        cmd.contains(&format!("{} ", t)) || cmd.ends_with(t)
    } else { cmd.contains(pat) }
}

fn default_whitelist() -> Vec<String> {
    ["npm","pnpm","yarn","bun","cargo","rustup","rustc","pip","pip3",
     "uv","poetry","go","make","cmake","git","svn",
     "ls","cat","head","tail","less","more","find","grep","rg","ag",
     "echo","printf","cd","pwd","mkdir","touch","cp","mv",
     "which","where","type","uname","whoami","date","df","du","ps","top"]
    .into_iter().map(String::from).collect()
}

fn default_risk_patterns() -> Vec<RiskPattern> {
    vec![
        RiskPattern { pattern: "rm -rf / ".into(), message: "高危：删除根目录".into(), level: RiskLevel::High,
            suggestion: Some("考虑 rm -rf /tmp/build".into()) },
        RiskPattern { pattern: "rm -rf /*".into(), message: "高危：删除根目录下所有文件".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "mkfs".into(), message: "高危：格式化文件系统".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "dd if=".into(), message: "高危：磁盘直接写入".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "chmod 777 /".into(), message: "高危：根目录 777".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "| sh".into(), message: "中危：管道执行 sh".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查管道内容".into()) },
        RiskPattern { pattern: "| bash".into(), message: "中危：管道执行 bash".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查管道内容".into()) },
        RiskPattern { pattern: "sudo".into(), message: "中危：sudo 提权".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "passwd".into(), message: "中危：修改密码".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "kill -9".into(), message: "中危：SIGKILL".into(), level: RiskLevel::Medium,
            suggestion: Some("先 kill -15".into()) },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn whitelist_allows() { let g = CommandGuard::default();
        assert!(matches!(g.check("npm install"), GuardDecision::Allow));
        assert!(matches!(g.check("git status"), GuardDecision::Allow));
        assert!(matches!(g.check("cargo build"), GuardDecision::Allow));
        assert!(matches!(g.check("echo hello"), GuardDecision::Allow)); }
    #[test] fn high_risk_denied() { let g = CommandGuard::default();
        assert!(matches!(g.check("rm -rf / "), GuardDecision::Deny(_)));
        assert!(matches!(g.check("rm -rf /*"), GuardDecision::Deny(_)));
        assert!(matches!(g.check("mkfs.ext4 /dev/sda1"), GuardDecision::Deny(_))); }
    #[test] fn safe_subdir() { let g = CommandGuard::default();
        assert!(matches!(g.check("rm -rf /var/log"), GuardDecision::Allow));
        assert!(matches!(g.check("rm -rf /tmp/build"), GuardDecision::Allow)); }
    #[test] fn risk_trumps_whitelist() { let g = CommandGuard::default();
        assert!(matches!(g.check("echo hello | sh"), GuardDecision::Ask(_)),
            "risk should override whitelist");
        assert!(matches!(g.check("sudo rm /tmp/foo"), GuardDecision::Ask(_))); }
    #[test] fn custom_whitelist() { let mut g = CommandGuard::default();
        g.add_whitelist("myapp");
        assert!(matches!(g.check("myapp deploy"), GuardDecision::Allow)); }
    #[test] fn empty_is_allow() { let g = CommandGuard::default();
        assert!(matches!(g.check(""), GuardDecision::Allow)); }
}
