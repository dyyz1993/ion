/** 看板 —— 中栏，卡片网格 */
import { Box, Text } from "@opentui/core";
import { state } from "../state";
import { COLORS, statusIcon } from "../theme";

export function renderKanban(renderer: any): any {
  const focused = state.focusedPanel === "kanban";
  const borderColor = focused ? COLORS.borderFocused : COLORS.borderInactive;

  const children: any[] = [
    Text({
      content: ` Workers · ${state.workers.length} `,
      fg: COLORS.accent,
      bold: true,
    }),
  ];

  if (state.workers.length === 0) {
    children.push(
      Text({ content: "No active workers\nPress 'n' to create one", fg: COLORS.subtext })
    );
  }

  // 卡片网格（每行 2 张卡片，OpenTUI 会自动 wrap）
  const cardRows: any[] = [];
  for (const w of state.workers) {
    const { icon, color } = statusIcon(w.status);
    const uptime = formatUptime(w.started_at);
    const log = (w.log_short || "").slice(0, 50);

    cardRows.push(
      Box(
        {
          borderStyle: "rounded",
          borderColor: COLORS.borderNormal,
          bg: COLORS.panelBg,
          padding: 1,
          flexDirection: "column",
          flexGrow: 1,
          minWidth: 30,
        },
        [
          Text({ content: `${icon} ${w.agent}`, fg: color, bold: true }),
          Text({
            content: `${uptime} · ${w.status.toUpperCase()}`,
            fg: COLORS.subtext,
          }),
          Text({ content: w.model || "?", fg: COLORS.subtext }),
          log
            ? Text({ content: `▸ ${log}`, fg: COLORS.text })
            : Text({ content: "(no output yet)", fg: COLORS.subtext }),
        ]
      )
    );
  }

  if (cardRows.length > 0) {
    // 每行 2 张
    const rows: any[] = [];
    for (let i = 0; i < cardRows.length; i += 2) {
      const slice = cardRows.slice(i, i + 2);
      rows.push(
        Box({ flexDirection: "row", gap: 1, width: "100%" }, slice)
      );
    }
    children.push(Box({ flexDirection: "column", gap: 1, flexGrow: 1 }, rows));
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
  return `${pad(h)}:${pad(m)}:${pad(s)}`;
}

function pad(n: number): string {
  return String(n).padStart(2, "0");
}
