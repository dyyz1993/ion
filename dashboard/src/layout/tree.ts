/** 项目树 —— 左栏 */
import { Box, Text } from "@opentui/core";
import { state } from "../state";
import { COLORS } from "../theme";

export function renderTree(renderer: any): any {
  const focused = state.focusedPanel === "tree";
  const borderColor = focused ? COLORS.borderFocused : COLORS.borderInactive;

  const children: any[] = [
    Text({
      content: ` Projects · ${state.projects.length} `,
      fg: COLORS.accent,
      bold: true,
    }),
  ];

  for (const proj of state.projects) {
    const collapsed = state.collapsed.has(proj.name);
    const icon = collapsed ? "▶" : "▼";
    const isSelected = focused && false; // TODO: 选中状态

    children.push(
      Text({
        content: `${icon} ${proj.name} (${proj.worker_count})`,
        fg: isSelected ? COLORS.bg : COLORS.text,
        bg: isSelected ? COLORS.accent : undefined,
      })
    );

    // 展开时显示子 session
    if (!collapsed) {
      for (const w of state.workers) {
        if (w.project !== proj.name) continue;
        const { icon: stIcon, color: stColor } = statusIconLocal(w.status);
        const shortSid = w.session_id.slice(0, 8);
        const isSel = focused && state.selectedSessionId === w.session_id;
        children.push(
          Text({
            content: `  ${stIcon} ${shortSid} ${w.agent}`,
            fg: isSel ? COLORS.bg : stColor,
            bg: isSel ? COLORS.accent : undefined,
          })
        );
      }
    }
  }

  if (state.projects.length === 0) {
    children.push(Text({ content: "(no projects)", fg: COLORS.subtext }));
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

// 局部复用 statusIcon（避免循环引用）
function statusIconLocal(status: string) {
  switch (status) {
    case "busy": return { icon: "▶", color: COLORS.danger };
    case "idle": return { icon: "⏸", color: COLORS.accent };
    case "stale": return { icon: "⚠", color: COLORS.warning };
    case "dead": return { icon: "⨯", color: COLORS.dead };
    default: return { icon: "?", color: COLORS.subtext };
  }
}
