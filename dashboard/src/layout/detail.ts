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

  if (!worker) {
    return Box(
      {
        borderStyle: "rounded",
        borderColor,
        bg: COLORS.panelBg,
        padding: 1,
        flexGrow: 1,
      },
      [
        Text({ content: " Detail ", fg: COLORS.accent, bold: true }),
        Text({
          content: "Select a worker card\n(Tab to kanban, Enter to select)",
          fg: COLORS.subtext,
        }),
      ]
    );
  }

  const { icon, color } = statusIcon(worker.status);
  const children: any[] = [
    Text({ content: " Detail ", fg: COLORS.accent, bold: true }),
    Text({ content: `${icon} ${worker.agent}`, fg: color, bold: true }),
    Text({ content: worker.model || "?", fg: COLORS.subtext }),
  ];

  if (focusMode) {
    // Focus 模式：显示更多
    const uptime = formatUptime(worker.started_at);
    children.push(
      Text({ content: `${uptime} · ${worker.status.toUpperCase()}`, fg: COLORS.subtext })
    );
    children.push(Text({ content: "", fg: COLORS.subtext }));
    children.push(
      Text({
        content: `Session: ${worker.session_id.slice(0, 12)}...`,
        fg: COLORS.subtext,
      })
    );
    children.push(
      Text({ content: `Project: ${worker.project}`, fg: COLORS.subtext })
    );
    children.push(Text({ content: "", fg: COLORS.subtext }));
    children.push(Text({ content: "▸ Output", fg: COLORS.accent, bold: true }));
    if (worker.latest_output && worker.latest_output.length > 0) {
      for (const line of worker.latest_output.slice(-5)) {
        children.push(Text({ content: line.slice(0, 80), fg: COLORS.text }));
      }
    } else {
      children.push(Text({ content: "(no output yet)", fg: COLORS.subtext }));
    }
  } else {
    // 普通模式：紧凑
    const log = worker.log_short || "(no output)";
    children.push(Text({ content: "", fg: COLORS.subtext }));
    children.push(Text({ content: `▸ ${log.slice(0, 50)}`, fg: COLORS.text }));
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
