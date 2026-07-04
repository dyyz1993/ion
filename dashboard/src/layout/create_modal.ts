/** 创建 Worker 模态 —— n 键弹出（Phase D 完善 Input） */
import { Box, Text } from "@opentui/core";
import { state, rerender } from "../state";
import { COLORS } from "../theme";
import { createSession } from "../manager";

export function renderCreateModal(renderer: any): any {
  if (!state.createModal) return Text({ content: "" });

  const modal = state.createModal;

  return Box(
    {
      flexDirection: "row",
      width: "100%",
      height: "100%",
      alignItems: "center",
      justifyContent: "center",
    },
    [
      Box(
        {
          borderStyle: "rounded",
          borderColor: COLORS.accent,
          bg: COLORS.panelBg,
          padding: 2,
          flexDirection: "column",
          width: 60,
          gap: 1,
        },
        [
          Text({ content: "✦ Create New Worker", fg: COLORS.accent, bold: true }),
          Text({ content: "Worker 会在指定项目目录下工作", fg: COLORS.subtext }),
          Text({ content: `Path:  ${modal.path}`, fg: COLORS.text }),
          Text({ content: `Agent: ${modal.agent}`, fg: COLORS.text }),
          Text({
            content: `  Tab 切字段   Enter 创建   Esc 取消${modal.error ? `   ⚠ ${modal.error}` : ""}`,
            fg: modal.error ? COLORS.warning : COLORS.subtext,
          }),
        ]
      ),
    ]
  );
}
