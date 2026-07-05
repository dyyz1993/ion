/** 详情面板 —— 右栏 / Focus 模式 */
import { Box, Text } from "@opentui/core";
import { state } from "../state";
import { COLORS, statusIcon } from "../theme";

export function renderDetail(renderer: any, focusMode: boolean): any {
  const focused = state.focusedPanel === "detail";
  const borderColor = focused ? COLORS.borderFocused : COLORS.borderInactive;

  // 找到当前选中的 worker
  const worker = state.workers.find(
    (w) => w.session_id === state.selectedSessionId
  );

  const children: any[] = [
    Text({ content: " Detail ", fg: COLORS.accent, bold: true }),
  ];

  if (worker) {
    const { icon, color } = statusIcon(worker.status);
    children.push(Text({ content: `${icon} ${worker.agent}`, fg: color, bold: true }));
    children.push(Text({ content: worker.model || "?", fg: COLORS.subtext }));

    if (focusMode) {
      const uptime = formatUptime(worker.started_at);
      children.push(
        Text({ content: `${uptime} · ${worker.status.toUpperCase()}`, fg: COLORS.subtext })
      );
      children.push(Text({ content: `Project: ${worker.project}`, fg: COLORS.subtext }));
      children.push(Text({ content: "", fg: COLORS.subtext }));

      // 显示聊天历史（来自 state.messages）
      const msgs = state.messages.get(worker.session_id) || [];
      if (msgs.length > 0) {
        children.push(Text({ content: "▸ Chat", fg: COLORS.accent, bold: true }));
        // 显示最近 10 条
        for (const m of msgs.slice(-10)) {
          const prefix = m.role === "user" ? "你: " : "AI: ";
          const color = m.role === "user" ? COLORS.accent : COLORS.text;
          // 截断长消息
          const lines = m.content.split("\n").slice(0, 4);
          for (let i = 0; i < lines.length; i++) {
            const line = (i === 0 ? prefix : "    ") + lines[i].slice(0, 70);
            children.push(Text({ content: line, fg: color }));
          }
          if (m.streaming) {
            children.push(Text({ content: "▍", fg: COLORS.accent }));
          }
        }
      } else {
        children.push(Text({ content: "▸ Chat (empty)", fg: COLORS.subtext }));
        children.push(Text({ content: "Type a message below and press Enter", fg: COLORS.subtext }));
      }
    } else {
      const log = worker.log_short || "(no output)";
      children.push(Text({ content: "", fg: COLORS.subtext }));
      children.push(Text({ content: `▸ ${log.slice(0, 50)}`, fg: COLORS.text }));
    }
  } else {
    children.push(
      Text({
        content: "Select a worker card\n(Tab to kanban, Enter to select)",
        fg: COLORS.subtext,
      })
    );
  }

  // 日志容器（始终显示，放在详情底部）
  children.push(Text({ content: "", fg: COLORS.subtext }));
  children.push(Text({ content: "▸ Logs", fg: COLORS.accent, bold: true }));
  const recentLogs = state.logs.slice(0, 6);
  if (recentLogs.length === 0) {
    children.push(Text({ content: "(no logs)", fg: COLORS.subtext }));
  } else {
    for (const line of recentLogs) {
      children.push(Text({ content: line, fg: COLORS.subtext }));
    }
  }

  return Box(
    {
      borderStyle: "rounded",
      borderColor,
      bg: COLORS.panelBg,
      padding: 1,
      flexDirection: "column",
      flexGrow: 1,
    },
    children
  );
}

function formatUptime(startedAt?: number): string {
  if (!startedAt) return "--:--:--";
  const now = Date.now();
  const secs = Math.max(0, Math.floor((now - startedAt) / 1000));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  return `${String(h).padStart(2, "0")}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
}
