/** 赛博朋克调色板 */
export const colors = {
  bg: "#0a0e1a",
  panelBg: "#111827",
  text: "#c8d3f5",
  subtext: "#5b6b9c",
  accent: "#00ffd1",
  danger: "#ff2d95",
  warning: "#ffb800",
  dead: "#7a1f3d",
  borderFocused: "#00ffd1",
  borderNormal: "#1f4d5c",
  borderInactive: "#2a3349",
} as const;

export function statusIcon(s: string): { icon: string; color: string } {
  switch (s) {
    case "busy":  return { icon: "▶", color: colors.danger };
    case "idle":  return { icon: "⏸", color: colors.accent };
    case "stale": return { icon: "⚠", color: colors.warning };
    case "dead":  return { icon: "⨯", color: colors.dead };
    default:      return { icon: "?", color: colors.subtext };
  }
}
