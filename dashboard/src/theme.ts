/** 赛博朋克霓虹调色板 */

export const COLORS = {
  bg: "#0a0e1a",
  panelBg: "#111827",
  text: "#c8d3f5",
  subtext: "#5b6b9c",
  accent: "#00ffd1",      // 主强调（青）
  danger: "#ff2d95",      // 忙/危险（品红）
  warning: "#ffb800",     // stale 警告
  dead: "#7a1f3d",
  borderFocused: "#00ffd1",
  borderNormal: "#1f4d5c",
  borderInactive: "#2a3349",
} as const;

/** 状态对应的图标和颜色 */
export function statusIcon(status: string): { icon: string; color: string } {
  switch (status) {
    case "busy": return { icon: "▶", color: COLORS.danger };
    case "idle": return { icon: "⏸", color: COLORS.accent };
    case "stale": return { icon: "⚠", color: COLORS.warning };
    case "dead": return { icon: "⨯", color: COLORS.dead };
    default: return { icon: "?", color: COLORS.subtext };
  }
}
