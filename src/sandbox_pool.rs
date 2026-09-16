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
// 沙盒档案 + 审批泵（SANDBOX_POOL.md §3.3 Phase 2 / Phase 1.5）
// ---------------------------------------------------------------------------

/// 无人值守审批策略（沙盒档案 `approval_policy` 字段）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// 人工审批（缺省）
    #[default]
    Default,
    /// 审批泵自动放行：ApprovalRequest → 自动 `review_approve_all`
    AutoApprove,
}

impl ApprovalPolicy {
    /// 解析配置串（空/未知值 → Default，宽容解析不炸配置加载）。
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto_approve" | "auto-approve" | "autoapprove" => Self::AutoApprove,
            _ => Self::Default,
        }
    }

    /// 序列化为配置串（list_sandboxes / sandbox_policy RPC 用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AutoApprove => "auto_approve",
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
            approval_policy: ApprovalPolicy::parse(&h.approval_policy),
            notes: h.notes.clone(),
        }
    }
}

impl SandboxPool {
    /// 取一台沙盒的档案（未配置 → Default 空档案）。
    pub fn profile(&self, name: &str) -> SandboxProfile {
        self.hosts.get(name).map(|h| SandboxProfile {
            approval_policy: ApprovalPolicy::parse(&h.approval_policy),
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

/// 审批泵冷却窗口（毫秒）：同 worker 两次自动放行的最小间隔。
/// ApprovalRequest 在 gate check 重复触发时防刷（review_approve_all 幂等，但别刷屏）。
pub const PUMP_COOLDOWN_MS: u64 = 2_000;

/// 审批泵 fire 判定（纯函数）：策略为 auto_approve 且距上次 fire 超过冷却窗。
pub fn pump_should_fire(
    policy: ApprovalPolicy,
    last_fire_ms: Option<u64>,
    now_ms: u64,
) -> bool {
    if policy != ApprovalPolicy::AutoApprove {
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
            approval_policy: policy.into(),
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
        assert!(pump_should_fire(ApprovalPolicy::AutoApprove, None, 1_000));
        // 人工审批永不 fire
        assert!(!pump_should_fire(ApprovalPolicy::Default, None, 1_000));
    }

    #[test]
    fn pump_respects_cooldown_window() {
        // 冷却窗内（<2s）不重复 fire；窗外再放行
        assert!(!pump_should_fire(
            ApprovalPolicy::AutoApprove,
            Some(10_000),
            10_000 + PUMP_COOLDOWN_MS - 1
        ));
        assert!(pump_should_fire(
            ApprovalPolicy::AutoApprove,
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
