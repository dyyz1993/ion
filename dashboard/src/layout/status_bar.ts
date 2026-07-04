/** 状态栏 —— 底部一行 */
import { Box, Text } from "@opentui/core";
import { state } from "../state";
import { COLORS } from "../theme";

export function renderStatusBar(renderer: any): any {
  const connColor = state.connected ? COLORS.accent : COLORS.dead;
  const connText = state.connected ? "● connected" : "○ disconnected";

  const liveCount = state.workers.filter((w) => w.status !== "dead").length;

  return Box(
    {
      width: "100%",
      height: 1,
      bg: COLORS.bg,
      flexDirection: "row",
    },
    [
      Text({ content: ` ${connText} `, fg: connColor, bold: true }),
      Text({
        content: `${state.workers.length} workers | ${liveCount} live | stale: ${state.totalStale} | ${state.focusedPanel}  `,
        fg: COLORS.subtext,
      }),
      Text({
        content: "Tab:switch Enter:select n:new i:input d:focus q:quit",
        fg: COLORS.subtext,
      }),
    ]
  );
}
