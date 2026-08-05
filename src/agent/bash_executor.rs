//! L0 Bash Executor — spawn_watcher + 进程状态管理原语。
//!
//! 这是 pi 三层架构里的 L0 层（`packages/coding-agent/src/core/bash-executor.ts`）。
//! 上层 `bash.rs`（L1：BashRunTool/BashManageTool）通过这层执行命令、读流式输出、
//! 写日志、发完成通知。
//!
//! 当前实现：
//! - spawn_watcher：监控 child 进程的 stdout/stderr，定期 emit `process_output` 事件，
//!   进程结束后 emit `process_completed` 并通过 follow_up_tx 注入 `<bash_result>` 消息。
//!
//! 未来扩展（本次未做）：
//! - 引入 `BashSpawnStrategy` trait，让 SSH/容器扩展替换执行后端
//! - 把 emit_extension_event 改成接受 `&dyn ExtensionApi`，去掉 println! 全局副作用

use crate::agent::agent_loop::DeliverAs;
use crate::agent::bash::{
    emit_extension_event, now_ms, save_process_map_arc, FollowUpSender, NotifyMap, ProcessMap,
    StdinMap,
};
use ion_provider::types::{CustomContent, CustomMessage, Message};

/// Shared watcher task for background and foreground modes.
/// Reads stdout line by line, emits `process_output` events every ~1s,
/// writes to log file, and sends completion notification.
pub async fn spawn_watcher(
    map: ProcessMap,
    smap: StdinMap,
    nmap: NotifyMap,
    tx: Option<FollowUpSender>,
    pid: String,
    command: String,
    description: String,
    mut child: tokio::process::Child,
    mut stdin_rx: tokio::sync::mpsc::Receiver<String>,
    timeout: u64,
    cwd: String,
    session_id: String,
    deliver_as: DeliverAs,
) {
    let started = std::time::Instant::now();
    let log_dir = std::path::Path::new("/tmp").join("ion-bash");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{pid}.log"));

    // Forward stdin
    if let Some(mut child_stdin) = child.stdin.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(input) = stdin_rx.recv().await {
                let _ = child_stdin.write_all(input.as_bytes()).await;
                let _ = child_stdin.write_all(b"\n").await;
            }
        });
    }

    // Read stdout line by line via BufReader.
    // ★ stderr 合并在 bash.rs 的 spawn 处用 `exec 2>&1` 完成（shell 层重定向，
    //   比 Rust 层 Arc<Mutex> 简单且不会丢顺序）。
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut full_output = String::new();
    let mut line_buf: Vec<String> = Vec::new();
    let mut last_flush = std::time::Instant::now();
    let mut log_f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    let mut timed_out = false;
    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout).lines();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);

        loop {
            if std::time::Instant::now() >= deadline {
                timed_out = true;
                break; // overall timeout
            }

            // Try to read a line with 200ms timeout
            let line =
                tokio::time::timeout(std::time::Duration::from_millis(200), reader.next_line())
                    .await;

            match line {
                Ok(Ok(Some(text))) => {
                    full_output.push_str(&text);
                    full_output.push('\n');
                    if let Some(ref mut f) = log_f {
                        use std::io::Write;
                        let _ = writeln!(f, "{text}");
                    }
                    line_buf.push(text);
                    last_flush = std::time::Instant::now();
                }
                Ok(Ok(None)) => break, // EOF
                Ok(Err(_)) => break,   // read error
                Err(_) => {
                    // Timeout: flush pending output and continue
                    if !line_buf.is_empty() && last_flush.elapsed().as_secs() >= 1 {
                        let batch = line_buf.join("\n");
                        emit_extension_event(
                            "process_output",
                            &serde_json::json!({
                                "bid": pid, "output": batch, "lines": line_buf.len(),
                            }),
                        );
                        line_buf.clear();
                    }
                    continue;
                }
            }
        }
    }

    // Flush remaining output
    if !line_buf.is_empty() {
        let batch = line_buf.join("\n");
        emit_extension_event(
            "process_output",
            &serde_json::json!({
                "bid": pid, "output": batch, "lines": line_buf.len(),
            }),
        );
        line_buf.clear();
    }

    // Wait for the process to fully exit (collect exit code)
    // 如果超时了（timed_out=true），先 kill child 再 wait
    smap.lock().await.remove(&pid);
    let elapsed = started.elapsed().as_secs();
    let exit_status = if timed_out {
        let _ = child.kill().await;
        child.wait().await
    } else {
        child.wait().await
    };
    let (exit_code, event_type) = match exit_status {
        Ok(status) => {
            if timed_out {
                (None, "process_timeout")
            } else if status.success() {
                (status.code(), "process_completed")
            } else {
                (status.code(), "process_completed")
            }
        }
        Err(_) => (None, "process_error"),
    };
    let stdout_stderr = full_output.clone();

    // Write full output (should be redundant but safe)
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::io::Write;
        let _ = write!(f, "{}", stdout_stderr);
    }
    {
        let mut pm = map.lock().await;
        if let Some(entry) = pm.get_mut(&pid) {
            // 不要覆盖 bash_kill 标记的 killed 状态
            if entry.status != "killed" {
                entry.status = event_type.trim_start_matches("process_").to_string();
            }
            entry.exit_code = exit_code;
            entry.output = stdout_stderr.clone();
            entry.elapsed_secs = elapsed;
        }
    }
    save_process_map_arc(&map, &cwd, &session_id);
    nmap.lock().await.remove(&pid);

    emit_extension_event(
        event_type,
        &serde_json::json!({
            "bid": pid, "command": command, "description": description,
            "exit_code": exit_code, "elapsed_secs": elapsed, "log_path": log_path.to_string_lossy(),
            "reason": if exit_code == Some(0) { "completed" } else if exit_code.is_some() { "abnormal" } else { event_type.trim_start_matches("process_") },
        }),
    );

    if let Some(ref tx) = tx {
        // exit_code 友好格式化（按用户反馈迭代）：
        // - timed_out=true → "timeout"（我们主动 kill）
        // - exit_code = Some(n) → "n"（正常退出或失败）
        // - signal = Some(n) → "signal:N (SIGKILL/...)"（被信号杀死，比"unknown"具体）
        // - 都没有 → "unknown"（极少见，spawn 失败之类）
        //
        // 之前 exit="unknown" 包揽了「信号杀死」「spawn 失败」「wait 异常」三种情况，
        // 用户看不到具体原因。Unix 下 ExitStatus.signal() 能区分信号，加进来。
        let exit_code_str = if timed_out {
            "timeout".to_string()
        } else {
            match exit_code {
                Some(code) => code.to_string(),
                None => {
                    // Unix 下尝试拿信号号（被 SIGKILL/SIGTERM 杀死的情况）
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        if let Ok(status) = exit_status {
                            if let Some(sig) = status.signal() {
                                let name = match sig {
                                    1 => "SIGHUP",
                                    2 => "SIGINT",
                                    3 => "SIGQUIT",
                                    4 => "SIGILL",
                                    6 => "SIGABRT",
                                    9 => "SIGKILL",
                                    11 => "SIGSEGV",
                                    13 => "SIGPIPE",
                                    14 => "SIGALRM",
                                    15 => "SIGTERM",
                                    _ => "SIGNAL",
                                };
                                format!("signal:{} ({})", sig, name)
                            } else {
                                "unknown".to_string()
                            }
                        } else {
                            "unknown".to_string()
                        }
                    }
                    #[cfg(not(unix))]
                    "unknown".to_string()
                }
            }
        };
        // 格式精简：bid/exit/elapsed 放 XML 属性，content 只放进程输出（不重复 command）。
        // LLM 调 background=true 时已经知道命令内容，不需要在 result 里重复。
        // bid 让 LLM 能映射回是哪个后台进程。
        // ★ 输出截断策略：头 300 字节 + ...[truncated N bytes]... + 尾 200 字节。
        // 之前只截头（前 500 字节 + ...[truncated]），尾部信息丢失。
        // 但尾部往往更重要（错误信息、最终结果都在末尾）。
        let output_text = if stdout_stderr.len() > 500 {
            let head = &stdout_stderr[..300];
            let tail = &stdout_stderr[stdout_stderr.len() - 200..];
            let middle = stdout_stderr.len() - 500;
            format!("{}\n...[truncated {} bytes]...\n{}", head, middle, tail)
        } else {
            stdout_stderr.clone()
        };
        // ★ exit=signal:* / unknown（被 SIGKILL/SIGTERM 杀 / spawn 失败）+ 输出空时，
        // 给个 fallback 内容说明发生了什么，避免整个 body 空白。
        let output_text = if output_text.trim().is_empty()
            && (exit_code.is_none() || timed_out)
        {
            if timed_out {
                "(no output captured; process timed out and was killed by ion)".to_string()
            } else if exit_code_str.starts_with("signal:") {
                format!(
                    "(no output captured; process terminated by {} — \
                     likely OOM kill, manual kill, or system signal)",
                    exit_code_str
                )
            } else {
                "(no output captured; process spawn failed or wait error — \
                 no exit code and no signal captured)".to_string()
            }
        } else {
            output_text
        };
        let content = format!(
            "<bash_result bid=\"{}\" exit=\"{}\" elapsed=\"{}s\">\n{}\n</bash_result>",
            pid,
            exit_code_str,
            elapsed,
            output_text,
        );
        let msg = Message::Custom(CustomMessage {
            role: "custom".into(),
            custom_type: "bash_result".into(),
            content: CustomContent::Text(content),
            display: true,
            details: None,
            timestamp: now_ms(),
        });
        let _ = tx.send((msg, deliver_as));
    }
}

#[cfg(test)]
mod tests {

    /// 验证 Unix 下 ExitStatus::signal() 能区分信号类型。
    /// 用户反馈：「怎么还有未知的类型的？」——之前 exit="unknown" 包揽了
    /// 信号杀死/spawn 失败/wait 异常三种情况，现在用 signal() 区分。
    ///
    /// 这些测试证明底层 ExitStatusExt 在 SIGKILL/SIGTERM 等场景下
    /// 确实返回对应的 signal 号，bash_executor 的格式化逻辑能拿到。
    #[test]
    #[cfg(unix)]
    fn test_sigkill_produces_signal_9() {
        use std::os::unix::process::ExitStatusExt;
        // sh -c 'kill -9 $$' 让进程自杀（SIGKILL）
        let status = std::process::Command::new("sh")
            .args(["-c", "kill -9 $$"])
            .status()
            .expect("sh should spawn");
        assert!(
            !status.success(),
            "self-kill should not be success: {:?}",
            status
        );
        // code() 应该 None（被信号杀死，没正常退出）
        assert_eq!(status.code(), None, "SIGKILL -> code() should be None");
        // signal() 应该 Some(9)
        assert_eq!(status.signal(), Some(9), "SIGKILL -> signal() should be 9");
    }

    #[test]
    #[cfg(unix)]
    fn test_sigterm_produces_signal_15() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::Command::new("sh")
            .args(["-c", "kill -15 $$"])
            .status()
            .expect("sh should spawn");
        assert_eq!(status.code(), None);
        assert_eq!(status.signal(), Some(15), "SIGTERM -> signal() should be 15");
    }

    #[test]
    #[cfg(unix)]
    fn test_normal_exit_has_code_no_signal() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .status()
            .expect("sh should spawn");
        assert_eq!(status.code(), Some(0));
        assert_eq!(status.signal(), None, "normal exit -> signal() should be None");
    }

    #[test]
    #[cfg(unix)]
    fn test_nonzero_exit_has_code_no_signal() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 42"])
            .status()
            .expect("sh should spawn");
        assert_eq!(status.code(), Some(42));
        assert_eq!(status.signal(), None);
    }
}
