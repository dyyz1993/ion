/** Todo 面板占位（Phase D 完善） */
import { Box, Text } from "@opentui/core";
import { COLORS } from "../theme";

export function renderTodoPanel(renderer: any): any {
  return Box(
    {
      borderStyle: "rounded",
      borderColor: COLORS.borderInactive,
      bg: COLORS.panelBg,
      padding: 1,
      flexDirection: "column",
      flexGrow: 1,
    },
    [
      Text({ content: " Todo ", fg: COLORS.subtext, bold: true }),
      Text({ content: "(coming soon)", fg: COLORS.subtext }),
    ]
  );
}
