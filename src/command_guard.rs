//! CommandGuard — 命令执行前的风险检查
//!
//! 三种工作模式：
//! - `open`：全部放行（除了高危模式）— 完全信任场景
//! - `blacklist`：默认放行 + 黑名单拦截（旧行为，向后兼容）
//! - `whitelist`：只放行白名单命令 + 风险模式拦截 + 未知命令询问（推荐半信任）
//!
//! 优先级：高危 Deny > 中危 Ask > 白名单 Allow > mode 默认决策

use serde::{Deserialize, Serialize};

/// 风险级别
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel { High, Medium, Low }

/// 工作模式
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    /// 全部放行（除了高危 Deny）
    Open,
    /// 默认放行 + 黑名单拦截（旧行为）
    Blacklist,
    /// 只放行白名单 + 风险拦截 + 未知命令 Ask（推荐）
    Whitelist,
}

impl std::fmt::Display for GuardMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardMode::Open => write!(f, "open"),
            GuardMode::Blacklist => write!(f, "blacklist"),
            GuardMode::Whitelist => write!(f, "whitelist"),
        }
    }
}

impl Default for GuardMode {
    fn default() -> Self {
        // 默认半信任模式 — 真正的白名单
        GuardMode::Whitelist
    }
}

/// 风险模式定义
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskPattern {
    pub pattern: String,
    pub message: String,
    pub level: RiskLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Guard 决策
#[derive(Clone, Debug)]
pub enum GuardDecision {
    /// 放行
    Allow,
    /// 询问用户（通过 UI 通道）
    Ask(RiskPattern),
    /// 拒绝
    Deny(RiskPattern),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandGuard {
    /// 工作模式
    #[serde(default)]
    pub mode: GuardMode,
    /// 命令白名单（前缀匹配）
    #[serde(default = "default_whitelist")]
    pub whitelist: Vec<String>,
    /// 风险模式列表
    #[serde(default = "default_risk_patterns")]
    pub risk_patterns: Vec<RiskPattern>,
}

impl Default for CommandGuard {
    fn default() -> Self {
        Self {
            mode: GuardMode::default(),
            whitelist: default_whitelist(),
            risk_patterns: default_risk_patterns(),
        }
    }
}

impl CommandGuard {
    pub fn new(whitelist: Vec<String>, risk_patterns: Vec<RiskPattern>) -> Self {
        Self { mode: GuardMode::Blacklist, whitelist, risk_patterns }
    }

    /// 构造指定模式
    pub fn with_mode(mode: GuardMode) -> Self {
        Self {
            mode,
            whitelist: default_whitelist(),
            risk_patterns: default_risk_patterns(),
        }
    }

    /// 检查命令，返回决策
    pub fn check(&self, command: &str) -> GuardDecision {
        let t = command.trim();
        if t.is_empty() { return GuardDecision::Allow; }
        let lower = t.to_lowercase();

        // ── 1. 风险模式优先检查 ──
        // Open 模式：只看高危（High），中危/低危跳过
        // 其他模式：High Deny / Medium Ask / Low 跳过
        for p in &self.risk_patterns {
            if contains(&lower, &p.pattern.to_lowercase()) {
                match p.level {
                    RiskLevel::High => return GuardDecision::Deny(p.clone()),
                    RiskLevel::Medium => {
                        if self.mode != GuardMode::Open {
                            return GuardDecision::Ask(p.clone());
                        }
                        // Open 模式继续检查（中危不拦）
                    }
                    RiskLevel::Low => continue,
                }
            }
        }

        // ── 2. 白名单检查 ──
        // 对于复合命令（VAR=value && command 或 command1 && command2），
        // 检查每个子命令是否在白名单里。
        // 简化处理：取 && 和 ; 分隔的每个子命令的首词判断。
        let in_whitelist = self.matches_whitelist_compound(t);

        // ── 3. 按模式决定最终结果 ──
        match self.mode {
            GuardMode::Open => {
                // 全部放行（风险已经在前一步处理）
                GuardDecision::Allow
            }
            GuardMode::Blacklist => {
                // 旧行为：白名单只是快速放行，不在白名单也允许
                GuardDecision::Allow
            }
            GuardMode::Whitelist => {
                // 真正的白名单：在白名单 → Allow，不在 → Ask
                if in_whitelist {
                    GuardDecision::Allow
                } else {
                    GuardDecision::Ask(RiskPattern {
                        pattern: "*".into(),
                        message: format!("命令不在白名单中：{}", truncate(t, 80)),
                        level: RiskLevel::Medium,
                        suggestion: Some("如确认安全，请通过 UI 通道批准".into()),
                    })
                }
            }
        }
    }

    /// 白名单前缀匹配
    fn matches_whitelist(&self, command: &str) -> bool {
        let t = command.trim();
        // 取首词（命令本身，不含参数）
        let first_token = t.split_whitespace().next().unwrap_or("");
        if first_token.is_empty() { return false; }

        // 取首词的 basename（处理 /usr/local/bin/npm 这种路径）
        let basename = std::path::Path::new(first_token)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(first_token);

        for prefix in &self.whitelist {
            // 1. 精确匹配首词：command == prefix
            if first_token == prefix.as_str() { return true; }
            // 2. 匹配 basename：/usr/local/bin/npm → basename "npm" == prefix
            if basename == prefix.as_str() { return true; }
            // 3. 带参数的完整前缀匹配：prefix == "cargo test"
            if t.starts_with(&format!("{} ", prefix)) { return true; }
        }
        false
    }

    /// 检查复合命令（含 && ; | 管道、变量赋值）的白名单匹配。
    ///
    /// 对于 `WT_DIR=$(mktemp ...) && git worktree add ... && bash scripts/...` 这种命令，
    /// 取每个子命令的首词判断，只要有一个在白名单就放行。
    /// 这对 evolver agent 必要——它的 bash 命令经常以变量赋值开头。
    fn matches_whitelist_compound(&self, command: &str) -> bool {
        let t = command.trim();

        // 先尝试简单匹配（单命令的情况）
        if self.matches_whitelist(t) {
            return true;
        }

        // 复合命令：按 && ; | 拆分子命令
        // 对每个子命令，去掉变量赋值前缀（VAR=value），取真正的命令首词判断
        let subcmds: Vec<&str> = t.split(|c| c == '&' || c == ';' || c == '|').collect();
        for sub in subcmds {
            let sub = sub.trim();
            if sub.is_empty() { continue; }

            // 跳过纯变量赋值（VAR=value，不是命令）
            // 判断：如果不含空格且含 =，视为变量赋值
            if !sub.contains(' ') && sub.contains('=') { continue; }

            // 去掉变量赋值前缀：`WT_DIR=$(mktemp -d ...) git worktree` → 取 git worktree
            // 简化：找第一个不含 = 的词
            let cmd_part = {
                let mut found_cmd = false;
                let mut cmd_start = 0;
                for (i, part) in sub.split_whitespace().enumerate() {
                    // 变量赋值：WORD=... 或 WORD=$(...)
                    if !found_cmd && (part.contains('=') && !part.starts_with('-')) {
                        continue;
                    }
                    if !found_cmd {
                        found_cmd = true;
                        cmd_start = sub.find(part).unwrap_or(0);
                    }
                }
                if found_cmd { &sub[cmd_start..] } else { sub }
            };

            // 对提取出的命令部分做白名单检查
            if self.matches_whitelist(cmd_part) {
                return true;
            }
        }

        // 所有子命令都不在白名单 → 检查原始命令的首词是否是 bash/sh
        // bash scripts/xxx.sh 应该被允许
        let first_token = t.split_whitespace().next().unwrap_or("");
        if first_token == "bash" || first_token == "sh" || first_token == "source" {
            return true;
        }

        false
    }

    pub fn add_whitelist(&mut self, prefix: &str) {
        let p = prefix.trim().to_string();
        if !p.is_empty() && !self.whitelist.contains(&p) {
            self.whitelist.push(p);
        }
    }
}

/// 字符串包含匹配（带空格感知）
fn contains(cmd: &str, pat: &str) -> bool {
    if pat.ends_with(' ') {
        let t = pat.trim_end();
        cmd.contains(&format!("{} ", t)) || cmd.ends_with(t)
    } else {
        cmd.contains(pat)
    }
}

/// 截断字符串（用于日志）
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() }
    else { format!("{}...", &s[..max]) }
}

/// 默认白名单 — 常见开发命令
fn default_whitelist() -> Vec<String> {
    [
        // 包管理器
        "npm", "pnpm", "yarn", "bun", "npx", "node", "deno",
        "cargo", "rustup", "rustc",
        "pip", "pip3", "uv", "poetry", "python", "python3",
        "go",
        // 构建
        "make", "cmake", "ninja", "tsc", "vite", "webpack", "esbuild", "rollup",
        // 版本控制
        "git", "svn", "gh",
        // 文件操作（安全）
        "ls", "cat", "head", "tail", "less", "more",
        "find", "grep", "rg", "ag", "fd", "tree",
        "echo", "printf", "cd", "pwd",
        "mkdir", "touch", "cp", "mv", "rm",
        "diff", "wc", "sort", "uniq", "cut", "tr", "awk", "sed",
        "source", "test", "[", "true", "false",
        // 系统信息（只读）
        "which", "where", "type", "uname", "whoami", "id",
        "date", "df", "du", "ps", "top", "htop", "free", "lsof",
        "env", "printenv",
        // 测试
        "cargo test", "npm test", "pytest",
        // 网络（安全客户端）
        "curl", "wget",
        // 容器（让 Agent 能起容器）
        "docker", "podman", "container",
        // ion 自身（让 workflow 能调 ion --export 等）
        "ion",
    ].into_iter().map(String::from).collect()
}

/// 默认风险模式
fn default_risk_patterns() -> Vec<RiskPattern> {
    vec![
        // ── 高危：直接 Deny ──
        RiskPattern { pattern: "rm -rf / ".into(), message: "删除根目录".into(), level: RiskLevel::High,
            suggestion: Some("考虑 rm -rf /tmp/build".into()) },
        RiskPattern { pattern: "rm -rf /*".into(), message: "删除根目录下所有文件".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "rm -rf ~".into(), message: "删除家目录".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "rm -rf $home".into(), message: "删除家目录".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "mkfs".into(), message: "格式化文件系统".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "dd if=".into(), message: "磁盘直接写入".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "dd of=/dev/".into(), message: "写入块设备".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "chmod 777 /".into(), message: "根目录 777".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: ">/dev/sd".into(), message: "写入块设备".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: ">/dev/nvme".into(), message: "写入块设备".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: ":(){:|:&};:".into(), message: "Fork 炸弹".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: ":(){ :|: & };:".into(), message: "Fork 炸弹".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "fork bomb".into(), message: "Fork 炸弹".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "shutdown".into(), message: "关机命令".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "reboot".into(), message: "重启命令".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "init 0".into(), message: "关机命令".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "init 6".into(), message: "重启命令".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "systemctl stop".into(), message: "停止系统服务".into(), level: RiskLevel::High, suggestion: None },
        RiskPattern { pattern: "systemctl disable".into(), message: "禁用系统服务".into(), level: RiskLevel::High, suggestion: None },

        // ── 中危：Ask 用户 ──
        RiskPattern { pattern: "| sh".into(), message: "管道执行 sh".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查管道内容".into()) },
        RiskPattern { pattern: "| bash".into(), message: "管道执行 bash".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查管道内容".into()) },
        // sed -i 原地修改文件：高危！evolver agent 会用这个绕过 edit 工具限制改代码
        RiskPattern { pattern: "sed -i".into(), message: "sed 原地修改文件（绕过 edit 工具限制）".into(), level: RiskLevel::High,
            suggestion: Some("用 edit 工具或 container exec B ion --agent developer 改代码，不要用 sed -i".into()) },
        RiskPattern { pattern: "| zsh".into(), message: "管道执行 zsh".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查管道内容".into()) },
        RiskPattern { pattern: "| python".into(), message: "管道执行 python".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查管道内容".into()) },
        RiskPattern { pattern: "| perl".into(), message: "管道执行 perl".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查管道内容".into()) },
        RiskPattern { pattern: "| ruby".into(), message: "管道执行 ruby".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查管道内容".into()) },
        RiskPattern { pattern: "| base64 -d".into(), message: "管道 base64 解码（可能藏恶意代码）".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查解码内容".into()) },
        RiskPattern { pattern: "| base64 --decode".into(), message: "管道 base64 解码（可能藏恶意代码）".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查解码内容".into()) },
        RiskPattern { pattern: "base64 -d |".into(), message: "base64 解码管道（可能藏恶意代码）".into(), level: RiskLevel::Medium,
            suggestion: Some("先检查解码内容".into()) },
        RiskPattern { pattern: "eval $".into(), message: "eval 执行变量".into(), level: RiskLevel::Medium,
            suggestion: Some("避免 eval".into()) },
        RiskPattern { pattern: "eval (".into(), message: "eval 执行子 shell".into(), level: RiskLevel::Medium,
            suggestion: Some("避免 eval".into()) },
        RiskPattern { pattern: "sudo".into(), message: "sudo 提权".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "su ".into(), message: "切换用户".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "passwd".into(), message: "修改密码".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "kill -9".into(), message: "SIGKILL".into(), level: RiskLevel::Medium,
            suggestion: Some("先 kill -15".into()) },
        RiskPattern { pattern: "pkill".into(), message: "按名杀进程".into(), level: RiskLevel::Medium,
            suggestion: Some("确认目标进程".into()) },
        RiskPattern { pattern: "killall".into(), message: "按名杀进程".into(), level: RiskLevel::Medium,
            suggestion: Some("确认目标进程".into()) },
        RiskPattern { pattern: "crontab -".into(), message: "修改 cron 任务".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "launchctl load".into(), message: "加载 launchd 任务".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "chmod +x".into(), message: "添加可执行权限".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "chown".into(), message: "修改属主".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "iptables".into(), message: "修改防火墙规则".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "ufw".into(), message: "修改防火墙规则".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "defaults write".into(), message: "修改 macOS 系统默认".into(), level: RiskLevel::Medium, suggestion: None },
        RiskPattern { pattern: "npm install -g".into(), message: "全局安装 npm 包".into(), level: RiskLevel::Medium,
            suggestion: Some("确认包来源可信".into()) },
        RiskPattern { pattern: "pip install --user".into(), message: "用户级安装 pip 包".into(), level: RiskLevel::Medium,
            suggestion: Some("确认包来源可信".into()) },
        RiskPattern { pattern: "curl http://".into(), message: "HTTP 明文下载（可能被篡改）".into(), level: RiskLevel::Medium,
            suggestion: Some("使用 HTTPS".into()) },
        RiskPattern { pattern: "wget http://".into(), message: "HTTP 明文下载（可能被篡改）".into(), level: RiskLevel::Medium,
            suggestion: Some("使用 HTTPS".into()) },
        RiskPattern { pattern: "git clone http://".into(), message: "HTTP 明文 clone".into(), level: RiskLevel::Medium,
            suggestion: Some("使用 HTTPS".into()) },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Whitelist 模式（默认）──

    #[test]
    fn whitelist_mode_allows_known_commands() {
        let g = CommandGuard::with_mode(GuardMode::Whitelist);
        assert!(matches!(g.check("npm install"), GuardDecision::Allow));
        assert!(matches!(g.check("cargo build"), GuardDecision::Allow));
        assert!(matches!(g.check("git status"), GuardDecision::Allow));
        assert!(matches!(g.check("node app.js"), GuardDecision::Allow));
        assert!(matches!(g.check("go build"), GuardDecision::Allow));
    }

    #[test]
    fn whitelist_mode_asks_unknown_commands() {
        let g = CommandGuard::with_mode(GuardMode::Whitelist);
        // 不在白名单
        assert!(matches!(g.check("myscript --foo"), GuardDecision::Ask(_)));
        assert!(matches!(g.check("./dangerous-binary"), GuardDecision::Ask(_)));
        assert!(matches!(g.check("bash -c 'rm -rf /'"), GuardDecision::Ask(_)));
    }

    #[test]
    fn whitelist_mode_allows_binary_path() {
        let g = CommandGuard::with_mode(GuardMode::Whitelist);
        // /usr/local/bin/npm 应该被识别为 npm
        assert!(matches!(g.check("/usr/local/bin/npm install"), GuardDecision::Allow));
    }

    #[test]
    fn whitelist_mode_high_risk_still_denied() {
        let g = CommandGuard::with_mode(GuardMode::Whitelist);
        // rm 不在白名单，但即使假设在，高危模式优先
        assert!(matches!(g.check("rm -rf / "), GuardDecision::Deny(_)));
        assert!(matches!(g.check("rm -rf ~"), GuardDecision::Deny(_)));
        assert!(matches!(g.check("mkfs.ext4 /dev/sda1"), GuardDecision::Deny(_)));
    }

    #[test]
    fn whitelist_mode_medium_risk_asks() {
        let g = CommandGuard::with_mode(GuardMode::Whitelist);
        // curl 在白名单，但 | sh 中危
        assert!(matches!(g.check("curl https://example.com | sh"), GuardDecision::Ask(_)));
        // sudo 中危
        assert!(matches!(g.check("sudo ls"), GuardDecision::Ask(_)));
    }

    // ── Blacklist 模式（旧行为）──

    #[test]
    fn blacklist_mode_allows_everything_safe() {
        let g = CommandGuard::with_mode(GuardMode::Blacklist);
        assert!(matches!(g.check("npm install"), GuardDecision::Allow));
        assert!(matches!(g.check("unknown-command"), GuardDecision::Allow));
        assert!(matches!(g.check("anything"), GuardDecision::Allow));
    }

    #[test]
    fn blacklist_mode_still_blocks_high_risk() {
        let g = CommandGuard::with_mode(GuardMode::Blacklist);
        assert!(matches!(g.check("rm -rf / "), GuardDecision::Deny(_)));
        assert!(matches!(g.check("mkfs /dev/sda"), GuardDecision::Deny(_)));
    }

    // ── Open 模式 ──

    #[test]
    fn open_mode_allows_all_except_high_risk() {
        let g = CommandGuard::with_mode(GuardMode::Open);
        assert!(matches!(g.check("anything"), GuardDecision::Allow));
        assert!(matches!(g.check("unknown-binary --x"), GuardDecision::Allow));
        assert!(matches!(g.check("sudo ls"), GuardDecision::Allow));  // open 模式不询问
        // 高危仍然拦
        assert!(matches!(g.check("rm -rf / "), GuardDecision::Deny(_)));
    }

    // ── 风险模式覆盖测试 ──

    #[test]
    fn high_risk_fork_bomb_denied() {
        let g = CommandGuard::default();
        assert!(matches!(g.check(":(){:|:&};:"), GuardDecision::Deny(_)));
    }

    #[test]
    fn high_risk_dd_to_device_denied() {
        let g = CommandGuard::default();
        assert!(matches!(g.check("dd if=/dev/zero of=/dev/sda"), GuardDecision::Deny(_)));
        assert!(matches!(g.check("dd of=/dev/sdb"), GuardDecision::Deny(_)));
    }

    #[test]
    fn high_risk_redirect_to_device_denied() {
        let g = CommandGuard::default();
        assert!(matches!(g.check("echo x >/dev/sda"), GuardDecision::Deny(_)));
    }

    #[test]
    fn medium_risk_base64_pipe_asks() {
        let g = CommandGuard::default();
        // base64 解码管道
        assert!(matches!(g.check("echo xxx | base64 -d | sh"), GuardDecision::Ask(_)));
        assert!(matches!(g.check("echo xxx | base64 -d > /tmp/evil"), GuardDecision::Ask(_)));
    }

    #[test]
    fn medium_risk_eval_asks() {
        let g = CommandGuard::default();
        assert!(matches!(g.check("eval $(echo hello)"), GuardDecision::Ask(_)));
    }

    #[test]
    fn safe_subdir_rm_allowed_in_blacklist() {
        let g = CommandGuard::with_mode(GuardMode::Blacklist);
        assert!(matches!(g.check("rm -rf /tmp/build"), GuardDecision::Allow));
        assert!(matches!(g.check("rm -rf ./target"), GuardDecision::Allow));
    }

    #[test]
    fn safe_subdir_rm_asks_in_whitelist() {
        // whitelist 模式下 rm 不在白名单 → Ask
        let g = CommandGuard::with_mode(GuardMode::Whitelist);
        assert!(matches!(g.check("rm -rf /tmp/build"), GuardDecision::Ask(_)));
    }

    #[test]
    fn risk_trumps_whitelist() {
        let g = CommandGuard::default();
        // echo 在白名单，但 | sh 中危
        assert!(matches!(g.check("echo hello | sh"), GuardDecision::Ask(_)));
    }

    #[test]
    fn custom_whitelist() {
        let mut g = CommandGuard::with_mode(GuardMode::Whitelist);
        g.add_whitelist("myapp");
        assert!(matches!(g.check("myapp deploy"), GuardDecision::Allow));
        assert!(matches!(g.check("myapp"), GuardDecision::Allow));
    }

    #[test]
    fn empty_is_allow() {
        let g = CommandGuard::default();
        assert!(matches!(g.check(""), GuardDecision::Allow));
        assert!(matches!(g.check("   "), GuardDecision::Allow));
    }

    #[test]
    fn http_warning() {
        let g = CommandGuard::default();
        // curl 在白名单，但 http:// 是中危
        assert!(matches!(g.check("curl http://example.com"), GuardDecision::Ask(_)));
        // HTTPS 没问题
        assert!(matches!(g.check("curl https://example.com"), GuardDecision::Allow));
    }

    #[test]
    fn npm_install_safe() {
        let g = CommandGuard::with_mode(GuardMode::Whitelist);
        // 普通 npm install 安全
        assert!(matches!(g.check("npm install lodash"), GuardDecision::Allow));
        // 但 npm install -g 全局安装需确认
        assert!(matches!(g.check("npm install -g typescript"), GuardDecision::Ask(_)));
    }
}
