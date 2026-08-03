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

    // Read stdout line by line via BufReader
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
        // exit_code 是 Option<i32>，按 None/Some(0)/Some(N) 友好格式化：
        // - None → "unknown"（未拿到退出码，比如 spawn 失败/超时）
        // - Some(0) → "0"
        // - Some(N) → "N"
        // 之前用 {:?} 直接 debug，会显示成 "None" / "Some(0)"，对前端不友好。
        let exit_code_str = if timed_out {
            "timeout".to_string()
        } else {
            match exit_code {
                None => "unknown".to_string(),
                Some(code) => code.to_string(),
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
