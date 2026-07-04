/**
 * 底部聊天输入框（真 InputRenderable，单例保留）
 *
 * 策略：Input 实例只创建一次，每次 render 复用。
 * rerender 时由 index.ts 负责 NOT destroy 它（通过保护引用）。
 */
import { Box, Text, InputRenderable, InputRenderableEvents } from "@opentui/core";
import { state, rerender, log } from "../state";
import { COLORS } from "../theme";
import { sendPrompt } from "../manager";

// 单例 Input 实例
export let inputInstance: InputRenderable | null = null;

export function getInputInstance(): InputRenderable | null {
  return inputInstance;
}

export function createInputBar(renderer: any): InputRenderable {
  if (inputInstance) return inputInstance;
  const input = new InputRenderable(renderer, {
    placeholder: "Type a message and press Enter... (i to focus)",
    value: state.inputValue,
    width: "100%",
  });

  input.on(InputRenderableEvents.INPUT, (val: string) => {
    state.inputValue = val || "";
  });

  input.on(InputRenderableEvents.ENTER, () => {
    const text = state.inputValue;
    if (!text) return;
    if (!state.selectedSessionId) {
      log("请先选中一个 worker（Tab → kanban → Enter）");
      rerender();
      return;
    }
    const sid = state.selectedSessionId;
    log(`发送到 ${sid.slice(0, 8)}: ${text.slice(0, 30)}`);
    sendPrompt(sid, text).catch((e) => log(`发送失败: ${e.message}`));
    state.inputValue = "";
    input.value = "";
    rerender();
  });

  inputInstance = input;
  log("input 实例已创建");
  return input;
}

export function renderInputBar(renderer: any): any {
  const focused = state.focusedPanel === "input";
  const borderColor = focused ? COLORS.borderFocused : COLORS.borderInactive;

  const input = createInputBar(renderer);

  if (focused) input.focus();
  else input.blur();

  if (input.value !== state.inputValue) {
    input.value = state.inputValue;
  }

  return Box(
    {
      borderStyle: "rounded",
      borderColor,
      bg: COLORS.panelBg,
      padding: 1,
      width: "100%",
      height: 3,
    },
    [input as any]
  );
}

