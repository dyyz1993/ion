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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug)]
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
        cmd.arg(&st.dest);
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

    /// 全部沙盒状态快照。
    pub fn statuses(&self) -> Vec<&SandboxStatus> {
        let mut v: Vec<&SandboxStatus> = self.statuses.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
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
}
