/** 输入框 —— 底部聊天输入（暂时用 Text 占位，Input 单独处理） */
import { Box, Text } from "@opentui/core";
import { state, rerender } from "../state";
import { COLORS } from "../theme";
import { sendPrompt } from "../manager";

export function renderInputBar(renderer: any): any {
  const focused = state.focusedPanel === "input";
  const borderColor = focused ? COLORS.borderFocused : COLORS.borderInactive;

  // Phase C 先用 Text 占位，Phase D 接入真正的 Input
  return Box(
    {
      borderStyle: "rounded",
      borderColor,
      bg: COLORS.panelBg,
      padding: 1,
      width: "100%",
      height: 3,
    },
    [
      Text({
        content: state.inputValue || "Type a message... (i to focus)",
        fg: state.inputValue ? COLORS.text : COLORS.subtext,
      }),
    ]
  );
}
