//! SandboxPool — 沙盒池（remote_workers 的健康探测与选点）
//!
//! 数据来源：config.remote_workers（RemoteWorkerHost）。池内每台沙盒一份
//! SandboxStatus；`probe` 经 ssh 直连形态跑 `<worker_bin> --version` 探测，
//! `pick_healthy` 按"健康优先 + 负载均摊 + exclude"选点。
//!
//! wrapper 端点（Windows+WSL 经 cmd 包装）本期不做探测，health 保持 Unknown。

use crate::config::IonConfig;
use std::collections::HashMap;
use std::time::Duration;

/// 沙盒健康状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxHealth {
    /// 未探测过（含 wrapper 端点——本期不探测）
    Unknown,
    /// ssh 直连可达且版本匹配
    Reachable,
    /// ssh 连不通 / 命令超时 / 非零退出
    Unreachable,
    /// 可达但 worker 版本与本端不一致
    VersionMismatch,
}

/// 单台沙盒的状态快照。
#[derive(Clone, Debug, serde::Serialize)]
pub struct SandboxStatus {
    /// 配置名（remote_workers 的 key）
    pub name: String,
    /// 展示串：`user@hostname:port`（user 空则 hostname 原样）
    pub dest: String,
    pub health: SandboxHealth,
    /// 最近一次探测到的远端 worker 版本（`ion --version` 输出）
    pub version: Option<String>,
    /// 当前在该沙盒上运行的 worker 数
    pub worker_count: usize,
    /// 最近一次探测时间（unix 秒）
    pub last_probe: Option<u64>,
}

/// 沙盒池：持有全部 remote_workers 的状态（含各自的 SSH 配置）。
#[derive(Default)]
pub struct SandboxPool {
    statuses: HashMap<String, SandboxStatus>,
    hosts: HashMap<String, crate::config::RemoteWorkerHost>,
}

impl SandboxPool {
    /// 从配置构建（读取 `config.remote_workers`）。
    pub fn from_config(cfg: &IonConfig) -> Self {
        let mut statuses = HashMap::new();
        let mut hosts = HashMap::new();
        if let Some(workers) = &cfg.remote_workers {
            for (name, host) in workers {
                let user = if host.user.is_empty() {
                    host.hostname.clone()
                } else {
                    format!("{}@{}", host.user, host.hostname)
                };
                let dest = match host.port {
                    Some(p) if p != 22 => format!("{}:{}", user, p),
                    _ => user,
                };
                hosts.insert(name.clone(), host.clone());
                statuses.insert(
                    name.clone(),
                    SandboxStatus {
                        name: name.clone(),
                        dest,
                        health: SandboxHealth::Unknown,
                        version: None,
                        worker_count: 0,
                        last_probe: None,
                    },
                );
            }
        }
        // env 注入（ION_REMOTE_WORKERS）合并——env 优先（对齐 remote_worker_host 的解析顺序）
        if let Ok(env_json) = std::env::var("ION_REMOTE_WORKERS")
            && !env_json.trim().is_empty()
            && let Ok(m) =
                serde_json::from_str::<HashMap<String, crate::config::RemoteWorkerHost>>(&env_json)
        {
            for (name, host) in m {
                let user = if host.user.is_empty() {
                    host.hostname.clone()
                } else {
                    format!("{}@{}", host.user, host.hostname)
                };
                let dest = match host.port {
                    Some(p) if p != 22 => format!("{}:{}", user, p),
                    _ => user,
                };
                hosts.insert(name.clone(), host.clone());
                statuses.insert(
                    name.clone(),
                    SandboxStatus {
                        name,
                        dest,
                        health: SandboxHealth::Unknown,
                        version: None,
                        worker_count: 0,
                        last_probe: None,
                    },
                );
            }
        }
        Self { statuses, hosts }
    }

    /// 探测一台沙盒（ssh 直连形态：`ssh ... user@host -- <worker_bin> --version`）。
    ///
    /// - 5s ConnectTimeout + 5s 整体超时
    /// - 连不通 / 超时 / 失败 → Unreachable
    /// - 版本输出与 `env!("CARGO_PKG_VERSION")` 不一致 → VersionMismatch
    /// - wrapper 端点不探测（传入名字不存在或为 wrapper 形态时保持 Unknown）
    pub async fn probe(&mut self, name: &str) -> SandboxStatus {
        let Some(cfg) = crate::config::IonConfig::load().remote_worker_host(name) else {
            // 无配置：直接返回现有快照（无则 Unknown 空 status）
            return self.statuses.get(name).cloned().unwrap_or(SandboxStatus {
                name: name.to_string(),
                dest: name.to_string(),
                health: SandboxHealth::Unknown,
                version: None,
                worker_count: 0,
                last_probe: None,
            });
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let st = self.statuses.entry(name.to_string()).or_insert_with(|| {
            let user = if cfg.user.is_empty() {
                cfg.hostname.clone()
            } else {
                format!("{}@{}", cfg.user, cfg.hostname)
            };
            SandboxStatus {
                name: name.to_string(),
                dest: user,
                health: SandboxHealth::Unknown,
                version: None,
                worker_count: 0,
                last_probe: None,
            }
        });

        // wrapper 端点本期不探测
        if !cfg.wrapper.is_empty() {
            return st.clone();
        }

        let mut cmd = tokio::process::Command::new("ssh");
        cmd.arg("-o").arg("ConnectTimeout=5")
            .arg("-o").arg("StrictHostKeyChecking=accept-new")
            .arg("-o").arg("BatchMode=yes");
        if let Some(p) = cfg.port {
            cmd.arg("-p").arg(p.to_string());
        }
        if !cfg.key.is_empty() {
            cmd.arg("-i").arg(&cfg.key);
        }
        // ssh 目标必须 user@hostname（端口走 -p）——st.dest 是展示串，含 ":port" 会被
        // ssh 当主机名解析（scp 语法）导致永远 Unreachable（2026-09-13 实测踩坑）
        let ssh_dest = if cfg.user.is_empty() {
            cfg.hostname.clone()
        } else {
            format!("{}@{}", cfg.user, cfg.hostname)
        };
        cmd.arg(ssh_dest);
        let bin = if cfg.worker_bin.is_empty() {
            "/usr/local/bin/ion"
        } else {
            &cfg.worker_bin
        };
        cmd.arg("--").arg(bin).arg("--version");
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());
        cmd.stdin(std::process::Stdio::null());

        let output = tokio::time::timeout(Duration::from_secs(5), cmd.output()).await;

        match output {
            Ok(Ok(out)) if out.status.success() => {
                let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let ver_clean = ver
                    .strip_prefix("ion ")
                    .or_else(|| ver.strip_prefix("ion"))
                    .unwrap_or(&ver)
                    .trim()
                    .to_string();
                let expected = env!("CARGO_PKG_VERSION");
                st.health = if ver_clean == expected {
                    SandboxHealth::Reachable
                } else {
                    SandboxHealth::VersionMismatch
                };
                st.version = if ver_clean.is_empty() { None } else { Some(ver_clean) };
            }
            Ok(Ok(out)) if !String::from_utf8_lossy(&out.stdout).trim().is_empty()
                && out.status.code() == Some(0) =>
            {
                st.health = SandboxHealth::Reachable;
                st.version = None;
            }
            _ => {
                st.health = SandboxHealth::Unreachable;
                st.version = None;
            }
        }
        st.last_probe = Some(now);
        st.clone()
    }

    /// 选一台沙盒：Reachable 优先，同健康度取 worker_count 最少；
    /// exclude 命中跳过；全不健康（或全被 exclude）返回 None。
    pub fn pick_healthy(&self, exclude: &[&str]) -> Option<&str> {
        self.statuses
            .values()
            .filter(|s| s.health == SandboxHealth::Reachable)
            .filter(|s| !exclude.contains(&s.name.as_str()))
            .min_by_key(|s| (s.worker_count, s.name.clone()))
            .map(|s| s.name.as_str())
    }

    /// 顺序探测池内全部沙盒（auto 选点前调用；每台 5s 超时上限）。
    pub async fn probe_all(&mut self) {
        let names: Vec<String> = self.statuses.keys().cloned().collect();
        for n in names {
            self.probe(&n).await;
        }
    }

    /// 全部沙盒状态快照。
    /// 池内是否存在该沙盒（probe 前置校验用）
    pub fn contains(&self, name: &str) -> bool {
        self.statuses.contains_key(name)
    }

    /// 可变状态访问（host 侧注入 worker_count 等运行时数据）
    pub fn statuses_mut(&mut self) -> impl Iterator<Item = &mut SandboxStatus> {
        self.statuses.values_mut()
    }

    pub fn statuses(&self) -> Vec<&SandboxStatus> {
        let mut v: Vec<&SandboxStatus> = self.statuses.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

/// auto-respawn 的沙盒故障转移决策结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailoverDecision {
    /// 原 host 不在池内（本地 worker / 显式指定但未定义的 host）→ 保持原 host
    NotPoolMember,
    /// 池成员：探测后选出健康节点（可能是原 host 自身——心跳误杀且沙盒实际存活）
    Failover { to: String },
    /// 池成员但无任何健康节点 → 回落原 host（保持可重派性），调用方应 WARN
    NoHealthyFallback,
}

/// auto-respawn 故障转移决策（纯函数，供 mock 池单测）：
/// 原 host 是沙盒池成员（含 "auto"）时，按池内健康状态重新选点——
/// 治「worker 死亡重派硬编码回 record.host（原病沙盒）」：沙盒死了重派=送死。
/// 非 `auto` 且不在池内 → 不做 failover（零开销，保持既有行为）。
pub fn failover_decision(pool: &SandboxPool, original_host: &str) -> FailoverDecision {
    if original_host != "auto" && !pool.contains(original_host) {
        return FailoverDecision::NotPoolMember;
    }
    match pool.pick_healthy(&[]) {
        Some(picked) => FailoverDecision::Failover { to: picked.to_string() },
        None => FailoverDecision::NoHealthyFallback,
    }
}

// ---------------------------------------------------------------------------
// 沙盒档案 + 审批泵（SANDBOX_POOL.md §3.3 Phase 2 / Phase 1.5；M3 全来源升级）
// ---------------------------------------------------------------------------

/// 审批来源（统一审批总线三来源，M1/M2/M3 对齐基准）。
/// kind 字符串 = ApprovalRequest 事件 `data.kind` / approvals_pending 表 `kind`。
/// 协议形状固化在 schemas/rpc/subscribe/{approvals_pending,approval_respond}.json。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// file-snapshot 审批（file-approval 扩展；应答动词 review_approve_all / review_reject_all）
    FileSnapshot,
    /// 扩展问询（wasm host_ui_ask 等；应答走 ui_respond 通道）
    UiAsk,
    /// 远程动词授权转人工（VerbGate grants ask_on_deny；应答动词 verb_review）
    RemoteVerb,
}

impl ApprovalKind {
    /// 全部来源（展示/遍历用，顺序固定）。
    pub const ALL: [ApprovalKind; 3] = [Self::FileSnapshot, Self::UiAsk, Self::RemoteVerb];

    /// 解析 kind 字符串（snake_case / kebab-case 变体都认；None = 未知来源）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "file_snapshot" | "file-snapshot" => Some(Self::FileSnapshot),
            "ui_ask" | "ui-ask" => Some(Self::UiAsk),
            "remote_verb" | "remote-verb" => Some(Self::RemoteVerb),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileSnapshot => "file_snapshot",
            Self::UiAsk => "ui_ask",
            Self::RemoteVerb => "remote_verb",
        }
    }

    /// 从 ApprovalRequest 事件 data 提取来源：缺省/未知 → FileSnapshot。
    /// legacy file-approval 事件无 kind 字段（requestId/total/files 平铺）——
    /// 泵按 file_snapshot 语义处理，与升级前行为完全一致。
    pub fn from_event_data(data: Option<&serde_json::Value>) -> Self {
        data.and_then(|d| d.get("kind"))
            .and_then(|v| v.as_str())
            .and_then(Self::parse)
            .unwrap_or(Self::FileSnapshot)
    }
}

/// 单来源审批动作（per-kind 策略值）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    /// 人工审批（缺省）
    #[default]
    Ask,
    /// 审批泵自动放行
    Auto,
}

impl PolicyAction {
    /// 解析动作串：per-kind 词汇（auto/ask）与全局词汇（auto_approve/default）都认。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "auto_approve" | "auto-approve" | "autoapprove" => Some(Self::Auto),
            "ask" | "default" | "" => Some(Self::Ask),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
        }
    }
}

/// 配置里的 `approval_policy` 值（serde untagged 两形态，旧字符串配置零破坏）：
/// - `Str`：`"auto_approve"` / `"default"` / `""`（既有形态）
/// - `Map`：`{"file_snapshot":"auto","ui_ask":"ask","remote_verb":"auto"}`（M3 per-kind）
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ApprovalPolicySpec {
    Str(String),
    Map(serde_json::Map<String, serde_json::Value>),
}

impl Default for ApprovalPolicySpec {
    /// 缺省 = 空字符串（与升级前的 `String::new()` 语义一致：人工审批）。
    fn default() -> Self {
        Self::Str(String::new())
    }
}

/// 无人值守审批策略（沙盒档案 `approval_policy` 字段；M3 升级为 per-kind 覆盖）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// 人工审批（缺省）——全部来源 ask
    #[default]
    Default,
    /// 全放行——全部来源 auto（旧 "auto_approve" 语义不变）
    AutoApprove,
    /// per-kind 覆盖：只存 auto 集合（ask 条目归一化剔除；空集 ≡ Default）。
    /// 来源：map 形态配置 / map 形态运行时覆盖。
    PerKind(std::collections::HashSet<ApprovalKind>),
}

impl ApprovalPolicy {
    /// 宽容解析存储串（不炸配置加载）：scalar 词汇或 JSON map 字符串
    /// （`to_config_string` 的产物——record/override 表存的字符串两形态都吃）。
    /// 未知值 → Default（与升级前行为一致）。
    pub fn parse(raw: &str) -> Self {
        Self::parse_strict(raw).unwrap_or_default()
    }

    /// 从配置值（string | map 两形态）宽容解析。
    pub fn parse_spec(spec: &ApprovalPolicySpec) -> Self {
        match spec {
            ApprovalPolicySpec::Str(s) => Self::parse(s),
            ApprovalPolicySpec::Map(m) => {
                let mut autos = std::collections::HashSet::new();
                for (k, v) in m {
                    if let (Some(kind), Some(act)) =
                        (ApprovalKind::parse(k), v.as_str().and_then(PolicyAction::parse))
                        && act == PolicyAction::Auto
                    {
                        autos.insert(kind);
                    }
                }
                if autos.is_empty() {
                    Self::Default
                } else {
                    Self::PerKind(autos)
                }
            }
        }
    }

    /// 严格解析（RPC 写路径用，错误信息给用户）：
    /// - scalar：`auto_approve` / `default`（含连字符/大小写变体）
    /// - JSON map 字符串：key ∈ 三来源，value ∈ auto|ask（含全局词汇变体）
    /// 空 map / 全 ask → Default（归一化）。
    pub fn parse_strict(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(
                "missing 'policy' (auto_approve | default | {file_snapshot|ui_ask|remote_verb: auto|ask})"
                    .into(),
            );
        }
        if trimmed.starts_with('{') {
            let v: serde_json::Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("invalid policy map JSON: {e}"))?;
            let obj = v
                .as_object()
                .ok_or_else(|| "policy map must be a JSON object".to_string())?;
            if obj.is_empty() {
                return Ok(Self::Default);
            }
            let mut autos = std::collections::HashSet::new();
            for (k, val) in obj {
                let kind = ApprovalKind::parse(k).ok_or_else(|| {
                    format!("unknown policy kind '{k}' (expected file_snapshot | ui_ask | remote_verb)")
                })?;
                let act = val.as_str().and_then(PolicyAction::parse).ok_or_else(|| {
                    format!("unknown policy action for '{k}': {val} (expected auto | ask)")
                })?;
                if act == PolicyAction::Auto {
                    autos.insert(kind);
                }
            }
            return Ok(if autos.is_empty() {
                Self::Default
            } else {
                Self::PerKind(autos)
            });
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "auto_approve" | "auto-approve" | "autoapprove" => Ok(Self::AutoApprove),
            "default" => Ok(Self::Default),
            other => Err(format!(
                "unknown policy '{other}' (expected auto_approve | default | \
                 per-kind map {{file_snapshot|ui_ask|remote_verb: auto|ask}})"
            )),
        }
    }

    /// 指定来源的动作（审批泵判定入口）。
    pub fn action_for(&self, kind: ApprovalKind) -> PolicyAction {
        match self {
            Self::Default => PolicyAction::Ask,
            Self::AutoApprove => PolicyAction::Auto,
            Self::PerKind(autos) => {
                if autos.contains(&kind) {
                    PolicyAction::Auto
                } else {
                    PolicyAction::Ask
                }
            }
        }
    }

    /// 序列化为存储/配置串：scalar 形态原样；PerKind → 紧凑 JSON map（只含 auto 条目）。
    /// roundtrip 保证：`parse(to_config_string(p)) == p`。
    pub fn to_config_string(&self) -> String {
        match self {
            Self::Default => "default".into(),
            Self::AutoApprove => "auto_approve".into(),
            Self::PerKind(autos) => {
                let mut m = serde_json::Map::new();
                for kind in ApprovalKind::ALL {
                    if autos.contains(&kind) {
                        m.insert(kind.as_str().to_string(), serde_json::json!("auto"));
                    }
                }
                serde_json::Value::Object(m).to_string()
            }
        }
    }

    /// 展示/响应视图：scalar → "default"/"auto_approve"；PerKind → 三来源全覆盖对象
    /// （{"file_snapshot":"auto","ui_ask":"ask","remote_verb":"auto"}）。
    /// sandbox_policy RPC 的 effective/policy/profile/override 字段用这个。
    pub fn describe(&self) -> serde_json::Value {
        match self {
            Self::Default => serde_json::json!("default"),
            Self::AutoApprove => serde_json::json!("auto_approve"),
            Self::PerKind(_) => {
                let mut m = serde_json::Map::new();
                for kind in ApprovalKind::ALL {
                    m.insert(
                        kind.as_str().to_string(),
                        serde_json::json!(self.action_for(kind).as_str()),
                    );
                }
                serde_json::Value::Object(m)
            }
        }
    }

    /// 旧语义字符串（list_sandboxes 等遗留视图）：PerKind 无 scalar 等价 → "per_kind" 标记
    /// （结构化视图用 `describe()`）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AutoApprove => "auto_approve",
            Self::PerKind(_) => "per_kind",
        }
    }
}

/// 沙盒档案（三层模型的环境层）：审批策略 + 环境事实。
/// 来源：`remote_workers.<name>` 的 `approval_policy` / `notes` 字段；
/// probe 产出（版本等）后续并入这里，派发时消费。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxProfile {
    pub approval_policy: ApprovalPolicy,
    pub notes: Vec<String>,
}

impl SandboxProfile {
    /// 从沙盒配置（remote_workers.<name> 条目）提取档案。
    pub fn from_host(h: &crate::config::RemoteWorkerHost) -> Self {
        Self {
            approval_policy: ApprovalPolicy::parse_spec(&h.approval_policy),
            notes: h.notes.clone(),
        }
    }
}

impl SandboxPool {
    /// 取一台沙盒的档案（未配置 → Default 空档案）。
    pub fn profile(&self, name: &str) -> SandboxProfile {
        self.hosts.get(name).map(|h| SandboxProfile {
            approval_policy: ApprovalPolicy::parse_spec(&h.approval_policy),
            notes: h.notes.clone(),
        }).unwrap_or_default()
    }
}

/// 环境层 initial_prompt 前缀（派发时注入；档案无内容 → None 不注入）。
/// 只承载 notes 事实——审批策略走机制（审批泵），不进提示词。
pub fn profile_prompt(profile: &SandboxProfile) -> Option<String> {
    if profile.notes.is_empty() {
        return None;
    }
    let mut out = String::from("## 沙盒环境提示（沙盒档案自动注入，非任务内容）\n");
    for n in &profile.notes {
        out.push_str("- ");
        out.push_str(n.trim_end());
        out.push('\n');
    }
    Some(out)
}

/// 审批泵冷却窗口（毫秒）：同 worker 同来源两次自动放行的最小间隔。
/// ApprovalRequest 在 gate check 重复触发时防刷（review_approve_all 幂等，但别刷屏）。
/// 冷却键 = (worker_id, kind)：不同来源互不冷却（file_snapshot 放行不挡 remote_verb）。
pub const PUMP_COOLDOWN_MS: u64 = 2_000;

/// 审批泵 fire 判定（纯函数）：该来源动作为 auto 且距上次 fire 超过冷却窗。
pub fn pump_should_fire(
    policy: &ApprovalPolicy,
    kind: ApprovalKind,
    last_fire_ms: Option<u64>,
    now_ms: u64,
) -> bool {
    if policy.action_for(kind) != PolicyAction::Auto {
        return false;
    }
    match last_fire_ms {
        Some(t) => now_ms.saturating_sub(t) >= PUMP_COOLDOWN_MS,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteWorkerHost;

    fn host(hostname: &str) -> RemoteWorkerHost {
        RemoteWorkerHost {
            hostname: hostname.to_string(),
            ..Default::default()
        }
    }

    fn cfg_with(map: HashMap<String, RemoteWorkerHost>) -> IonConfig {
        IonConfig {
            remote_workers: Some(map),
            ..Default::default()
        }
    }

    fn make_status(name: &str, health: SandboxHealth, count: usize) -> SandboxStatus {
        SandboxStatus {
            name: name.to_string(),
            dest: format!("root@{}", name),
            health,
            version: None,
            worker_count: count,
            last_probe: None,
        }
    }

    fn pool_of(v: Vec<SandboxStatus>) -> SandboxPool {
        let mut m = HashMap::new();
        for s in v {
            m.insert(s.name.clone(), s);
        }
        SandboxPool { statuses: m, hosts: HashMap::new() }
    }

    #[test]
    fn empty_pool() {
        let pool = SandboxPool::default();
        assert!(pool.statuses().is_empty());
        assert_eq!(pool.pick_healthy(&[]), None);
    }

    #[test]
    fn from_config_parses() {
        let mut a = host("10.0.0.1");
        a.user = "root".into();
        a.port = Some(2222);
        let mut b = host("win38");
        b.user = "sshuser".into(); // port 缺省 → 不带 :22
        let mut m = HashMap::new();
        m.insert("a".to_string(), a);
        m.insert("b".to_string(), b);
        let pool = SandboxPool::from_config(&cfg_with(m));
        let sts = pool.statuses();
        assert_eq!(sts.len(), 2);
        let a = sts.iter().find(|s| s.name == "a").unwrap();
        assert_eq!(a.dest, "root@10.0.0.1:2222");
        assert_eq!(a.health, SandboxHealth::Unknown);
        let b = sts.iter().find(|s| s.name == "b").unwrap();
        assert_eq!(b.dest, "sshuser@win38");
    }

    #[test]
    fn pick_prefers_reachable_over_unhealthy() {
        let pool = pool_of(vec![
            make_status("down", SandboxHealth::Unreachable, 0),
            make_status("up", SandboxHealth::Reachable, 5),
            make_status("mismatch", SandboxHealth::VersionMismatch, 0),
            make_status("unknown", SandboxHealth::Unknown, 0),
        ]);
        assert_eq!(pool.pick_healthy(&[]), Some("up"));
    }

    #[test]
    fn pick_load_balances_by_worker_count() {
        let pool = pool_of(vec![
            make_status("busy", SandboxHealth::Reachable, 3),
            make_status("idle", SandboxHealth::Reachable, 0),
            make_status("mid", SandboxHealth::Reachable, 1),
        ]);
        assert_eq!(pool.pick_healthy(&[]), Some("idle"));
    }

    #[test]
    fn pick_respects_exclude() {
        let pool = pool_of(vec![
            make_status("idle", SandboxHealth::Reachable, 0),
            make_status("next", SandboxHealth::Reachable, 1),
        ]);
        assert_eq!(pool.pick_healthy(&["idle"]), Some("next"));
        assert_eq!(pool.pick_healthy(&["idle", "next"]), None);
    }

    #[test]
    fn pick_none_when_all_unhealthy() {
        let pool = pool_of(vec![
            make_status("a", SandboxHealth::Unknown, 0),
            make_status("b", SandboxHealth::Unreachable, 0),
            make_status("c", SandboxHealth::VersionMismatch, 0),
        ]);
        assert_eq!(pool.pick_healthy(&[]), None);
    }

    // ── auto-respawn 沙盒故障转移（fix4/h5-sandbox-failover）──

    #[test]
    fn failover_moves_to_healthy_node_when_original_dead() {
        // 重派场景：原 host（病沙盒）Unreachable，池里还有一台活的 → 必须换到活节点，
        // 不能固定回 record.host（原行为 = 沙盒死了重派 = 送死）。
        let pool = pool_of(vec![
            make_status("dead_node", SandboxHealth::Unreachable, 0),
            make_status("alive_node", SandboxHealth::Reachable, 2),
        ]);
        assert_eq!(
            failover_decision(&pool, "dead_node"),
            FailoverDecision::Failover {
                to: "alive_node".to_string()
            }
        );
    }

    #[test]
    fn failover_treats_auto_as_pool_member() {
        // host=="auto"（出生即 auto 的 record 兜底）也应参与池选点
        let pool = pool_of(vec![
            make_status("a", SandboxHealth::Unreachable, 0),
            make_status("b", SandboxHealth::Reachable, 0),
        ]);
        assert_eq!(
            failover_decision(&pool, "auto"),
            FailoverDecision::Failover { to: "b".to_string() }
        );
    }

    #[test]
    fn failover_falls_back_to_original_when_pool_all_dead() {
        // 池内无健康节点 → 回落原 host（保持可重派性），并让调用方 WARN
        let pool = pool_of(vec![
            make_status("a", SandboxHealth::Unreachable, 0),
            make_status("b", SandboxHealth::VersionMismatch, 0),
            make_status("c", SandboxHealth::Unknown, 0),
        ]);
        assert_eq!(
            failover_decision(&pool, "a"),
            FailoverDecision::NoHealthyFallback
        );
    }

    #[test]
    fn failover_skips_non_pool_member() {
        // 非池成员（显式 host 但不在 remote_workers / 本地 None→""）→ 不做 failover
        let pool = pool_of(vec![make_status(
            "other",
            SandboxHealth::Reachable,
            0,
        )]);
        assert_eq!(
            failover_decision(&pool, "some-explicit-host"),
            FailoverDecision::NotPoolMember
        );
        assert_eq!(failover_decision(&pool, ""), FailoverDecision::NotPoolMember);
    }

    #[test]
    fn failover_allows_healthy_original_still_picked() {
        // 原 host 探测后仍然健康（心跳误杀场景）：允许重新选中它（负载均摊可能选回）
        let pool = pool_of(vec![
            make_status("orig", SandboxHealth::Reachable, 0),
            make_status("busy", SandboxHealth::Reachable, 5),
        ]);
        assert_eq!(
            failover_decision(&pool, "orig"),
            FailoverDecision::Failover { to: "orig".to_string() }
        );
    }

    #[test]
    fn version_compare_decides_health() {
        // 纯逻辑判定：探到的版本串 == CARGO_PKG_VERSION → Reachable，否则 VersionMismatch
        let expected = env!("CARGO_PKG_VERSION");
        let mut s = make_status("x", SandboxHealth::Unknown, 0);
        assert_ne!(expected, "0.0.0-not-real");
        s.version = Some(expected.to_string());
        s.health = if s.version.as_deref() == Some(expected) {
            SandboxHealth::Reachable
        } else {
            SandboxHealth::VersionMismatch
        };
        assert_eq!(s.health, SandboxHealth::Reachable);
        s.version = Some("9.9.9".to_string());
        s.health = if s.version.as_deref() == Some(expected) {
            SandboxHealth::Reachable
        } else {
            SandboxHealth::VersionMismatch
        };
        assert_eq!(s.health, SandboxHealth::VersionMismatch);
    }

    // ── 沙盒档案 + 审批泵（SANDBOX_POOL.md §3.3 Phase 2）──

    fn host_with_policy(policy: &str, notes: Vec<String>) -> RemoteWorkerHost {
        RemoteWorkerHost {
            hostname: "h".into(),
            approval_policy: ApprovalPolicySpec::Str(policy.into()),
            notes,
            ..Default::default()
        }
    }

    #[test]
    fn approval_policy_parse_is_lenient() {
        assert_eq!(ApprovalPolicy::parse(""), ApprovalPolicy::Default);
        assert_eq!(ApprovalPolicy::parse("default"), ApprovalPolicy::Default);
        assert_eq!(
            ApprovalPolicy::parse("auto_approve"),
            ApprovalPolicy::AutoApprove
        );
        // 宽容：大小写/连字符变体也认；未知值回落 Default（不炸配置加载）
        assert_eq!(
            ApprovalPolicy::parse("Auto-Approve"),
            ApprovalPolicy::AutoApprove
        );
        assert_eq!(ApprovalPolicy::parse("whatever"), ApprovalPolicy::Default);
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::Default);
    }

    // ── M3 全来源升级：ApprovalKind / PolicyAction / per-kind 策略 ──

    #[test]
    fn approval_kind_parse_roundtrip() {
        for kind in ApprovalKind::ALL {
            assert_eq!(ApprovalKind::parse(kind.as_str()), Some(kind));
            // kebab-case 变体也认
            let kebab = kind.as_str().replace('_', "-");
            assert_eq!(ApprovalKind::parse(&kebab), Some(kind));
        }
        assert_eq!(ApprovalKind::parse("bogus"), None);
        assert_eq!(ApprovalKind::parse(""), None);
    }

    #[test]
    fn approval_kind_from_event_data_defaults_to_file_snapshot() {
        use serde_json::json;
        // 统一形态：带 kind
        assert_eq!(
            ApprovalKind::from_event_data(Some(&json!({"kind":"remote_verb","id":"x"}))),
            ApprovalKind::RemoteVerb
        );
        assert_eq!(
            ApprovalKind::from_event_data(Some(&json!({"kind":"ui_ask"}))),
            ApprovalKind::UiAsk
        );
        // legacy file-approval 形态：无 kind（requestId/total/files 平铺）→ file_snapshot
        assert_eq!(
            ApprovalKind::from_event_data(Some(&json!({"requestId":"appr_1","total":2,"files":[]}))),
            ApprovalKind::FileSnapshot
        );
        // 未知 kind / data 缺失 → file_snapshot（与升级前泵行为一致）
        assert_eq!(
            ApprovalKind::from_event_data(Some(&json!({"kind":"bogus"}))),
            ApprovalKind::FileSnapshot
        );
        assert_eq!(ApprovalKind::from_event_data(None), ApprovalKind::FileSnapshot);
    }

    #[test]
    fn policy_parse_map_form_per_kind() {
        // map 形态：只列的 auto 生效，未列 = ask
        let p = ApprovalPolicy::parse(r#"{"file_snapshot":"auto","ui_ask":"ask","remote_verb":"auto"}"#);
        assert_eq!(p.action_for(ApprovalKind::FileSnapshot), PolicyAction::Auto);
        assert_eq!(p.action_for(ApprovalKind::UiAsk), PolicyAction::Ask);
        assert_eq!(p.action_for(ApprovalKind::RemoteVerb), PolicyAction::Auto);
        // 全 ask / 空 map → 归一化 Default
        assert_eq!(ApprovalPolicy::parse(r#"{"ui_ask":"ask"}"#), ApprovalPolicy::Default);
        assert_eq!(ApprovalPolicy::parse(r#"{}"#), ApprovalPolicy::Default);
        // 坏 JSON → 宽容回落 Default（不炸配置加载）
        assert_eq!(ApprovalPolicy::parse("{not json"), ApprovalPolicy::Default);
    }

    #[test]
    fn policy_parse_accepts_global_vocab_in_map_values() {
        // map 值也认全局词汇（auto_approve/default）——两套词汇互通
        let p = ApprovalPolicy::parse(r#"{"file_snapshot":"auto_approve","ui_ask":"default"}"#);
        assert_eq!(p.action_for(ApprovalKind::FileSnapshot), PolicyAction::Auto);
        assert_eq!(p.action_for(ApprovalKind::UiAsk), PolicyAction::Ask);
    }

    #[test]
    fn policy_parse_spec_map_form() {
        use serde_json::json;
        // 配置 map 形态（untagged ApprovalPolicySpec::Map）
        let spec: ApprovalPolicySpec = serde_json::from_value(json!({
            "file_snapshot": "auto", "ui_ask": "ask", "remote_verb": "auto"
        })).unwrap();
        assert_eq!(
            spec,
            ApprovalPolicySpec::Map(
                vec![
                    ("file_snapshot".to_string(), json!("auto")),
                    ("ui_ask".to_string(), json!("ask")),
                    ("remote_verb".to_string(), json!("auto")),
                ]
                .into_iter()
                .collect()
            )
        );
        let p = ApprovalPolicy::parse_spec(&spec);
        assert_eq!(p.action_for(ApprovalKind::FileSnapshot), PolicyAction::Auto);
        assert_eq!(p.action_for(ApprovalKind::UiAsk), PolicyAction::Ask);
        // 旧字符串形态不受影响
        let s: ApprovalPolicySpec = serde_json::from_value(json!("auto_approve")).unwrap();
        assert_eq!(ApprovalPolicy::parse_spec(&s), ApprovalPolicy::AutoApprove);
        // 未知 kind / 非字符串值宽容忽略
        let noisy: ApprovalPolicySpec =
            serde_json::from_value(json!({"file_snapshot":"auto","bogus":"auto","ui_ask":true})).unwrap();
        let p2 = ApprovalPolicy::parse_spec(&noisy);
        assert_eq!(p2.action_for(ApprovalKind::FileSnapshot), PolicyAction::Auto);
        assert_eq!(p2.action_for(ApprovalKind::UiAsk), PolicyAction::Ask);
    }

    #[test]
    fn policy_parse_strict_errors() {
        // scalar 非法 → 明确报错（写路径不静默回落）
        assert!(ApprovalPolicy::parse_strict("yolo").is_err());
        assert!(ApprovalPolicy::parse_strict("").is_err());
        // map：未知 kind / 非法 value / 坏 JSON / 非对象 → 明确报错
        assert!(ApprovalPolicy::parse_strict(r#"{"bogus":"auto"}"#).is_err());
        assert!(ApprovalPolicy::parse_strict(r#"{"file_snapshot":"yolo"}"#).is_err());
        assert!(ApprovalPolicy::parse_strict(r#"{"file_snapshot":true}"#).is_err());
        assert!(ApprovalPolicy::parse_strict("{not json").is_err());
        assert!(ApprovalPolicy::parse_strict("[1,2]").is_err());
        // 合法两形态通过
        assert_eq!(ApprovalPolicy::parse_strict("auto_approve").unwrap(), ApprovalPolicy::AutoApprove);
        assert_eq!(ApprovalPolicy::parse_strict("default").unwrap(), ApprovalPolicy::Default);
        assert_eq!(
            ApprovalPolicy::parse_strict(r#"{"file_snapshot":"auto"}"#).unwrap(),
            ApprovalPolicy::PerKind([ApprovalKind::FileSnapshot].into_iter().collect())
        );
    }

    #[test]
    fn policy_to_config_string_roundtrip() {
        for p in [
            ApprovalPolicy::Default,
            ApprovalPolicy::AutoApprove,
            ApprovalPolicy::PerKind([ApprovalKind::FileSnapshot, ApprovalKind::RemoteVerb].into_iter().collect()),
            ApprovalPolicy::PerKind([ApprovalKind::UiAsk].into_iter().collect()),
        ] {
            let s = p.to_config_string();
            assert_eq!(ApprovalPolicy::parse(&s), p, "roundtrip 失败: {s}");
        }
        // scalar 形态的存储串保持旧词汇（旧数据/旧工具可读）
        assert_eq!(ApprovalPolicy::Default.to_config_string(), "default");
        assert_eq!(ApprovalPolicy::AutoApprove.to_config_string(), "auto_approve");
    }

    #[test]
    fn policy_describe_shapes() {
        use serde_json::json;
        assert_eq!(ApprovalPolicy::Default.describe(), json!("default"));
        assert_eq!(ApprovalPolicy::AutoApprove.describe(), json!("auto_approve"));
        // PerKind → 三来源全覆盖对象（未列 = ask）
        assert_eq!(
            ApprovalPolicy::PerKind([ApprovalKind::FileSnapshot, ApprovalKind::RemoteVerb].into_iter().collect()).describe(),
            json!({"file_snapshot":"auto","ui_ask":"ask","remote_verb":"auto"})
        );
        // 旧 as_str 视图：PerKind → "per_kind" 标记（list_sandboxes 遗留路径）
        assert_eq!(ApprovalPolicy::Default.as_str(), "default");
        assert_eq!(ApprovalPolicy::AutoApprove.as_str(), "auto_approve");
        assert_eq!(
            ApprovalPolicy::PerKind([ApprovalKind::UiAsk].into_iter().collect()).as_str(),
            "per_kind"
        );
    }

    #[test]
    fn pump_matrix_per_kind_policy() {
        use ApprovalKind as K;
        // 三 kind × 三策略的放行矩阵
        let map = ApprovalPolicy::PerKind([K::FileSnapshot, K::RemoteVerb].into_iter().collect());
        let cases: &[(ApprovalPolicy, K, bool)] = &[
            (ApprovalPolicy::Default, K::FileSnapshot, false),
            (ApprovalPolicy::Default, K::UiAsk, false),
            (ApprovalPolicy::Default, K::RemoteVerb, false),
            (ApprovalPolicy::AutoApprove, K::FileSnapshot, true),
            (ApprovalPolicy::AutoApprove, K::UiAsk, true),
            (ApprovalPolicy::AutoApprove, K::RemoteVerb, true),
            // map：列的 auto 放行、未列的 ask 不放行
            (map.clone(), K::FileSnapshot, true),
            (map.clone(), K::UiAsk, false),
            (map, K::RemoteVerb, true),
        ];
        for (i, (policy, kind, expect_fire)) in cases.iter().enumerate() {
            assert_eq!(
                pump_should_fire(policy, *kind, None, 1_000),
                *expect_fire,
                "矩阵 case #{i}: {policy:?} kind={kind:?}"
            );
        }
    }

    #[test]
    fn profile_reads_config_fields() {
        let mut m = HashMap::new();
        m.insert(
            "unattended".to_string(),
            host_with_policy("auto_approve", vec!["cargo 在 ~/.cargo/bin".into()]),
        );
        m.insert("manual".to_string(), host_with_policy("", vec![]));
        let pool = SandboxPool::from_config(&cfg_with(m));

        let p = pool.profile("unattended");
        assert_eq!(p.approval_policy, ApprovalPolicy::AutoApprove);
        assert_eq!(p.notes.len(), 1);
        // 未配置 / 不存在的沙盒 → Default 空档案（不 panic）
        assert_eq!(pool.profile("manual").approval_policy, ApprovalPolicy::Default);
        assert_eq!(pool.profile("nope"), SandboxProfile::default());
    }

    #[test]
    fn profile_prompt_only_for_notes() {
        assert_eq!(profile_prompt(&SandboxProfile::default()), None);
        let p = SandboxProfile {
            approval_policy: ApprovalPolicy::AutoApprove,
            notes: vec!["node 在 /usr/local/bin".into()],
        };
        let prompt = profile_prompt(&p).expect("notes 非空应有前缀");
        assert!(prompt.contains("沙盒环境提示"));
        assert!(prompt.contains("node 在 /usr/local/bin"));
        // 审批策略不进提示词（走机制泵，不走提示词）
        assert!(!prompt.contains("auto_approve"));
    }

    #[test]
    fn pump_fires_only_for_auto_approve() {
        assert!(pump_should_fire(
            &ApprovalPolicy::AutoApprove,
            ApprovalKind::FileSnapshot,
            None,
            1_000
        ));
        // 人工审批永不 fire
        assert!(!pump_should_fire(
            &ApprovalPolicy::Default,
            ApprovalKind::FileSnapshot,
            None,
            1_000
        ));
    }

    #[test]
    fn pump_respects_cooldown_window() {
        // 冷却窗内（<2s）不重复 fire；窗外再放行
        assert!(!pump_should_fire(
            &ApprovalPolicy::AutoApprove,
            ApprovalKind::FileSnapshot,
            Some(10_000),
            10_000 + PUMP_COOLDOWN_MS - 1
        ));
        assert!(pump_should_fire(
            &ApprovalPolicy::AutoApprove,
            ApprovalKind::FileSnapshot,
            Some(10_000),
            10_000 + PUMP_COOLDOWN_MS
        ));
    }

    #[test]
    fn env_injected_profile_flows_through() {
        // env 注入（ION_REMOTE_WORKERS）与 config 同构：档案字段一并生效。
        // from_config 读 env 走 std::env::var——这里只验证 hosts map 直构路径。
        let mut pool = SandboxPool::default();
        pool.hosts.insert(
            "ci".to_string(),
            host_with_policy("auto_approve", vec![]),
        );
        assert_eq!(pool.profile("ci").approval_policy, ApprovalPolicy::AutoApprove);
    }
}
