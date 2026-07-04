/** 全局键盘快捷键 */
import { state, rerender, switchSession, log } from "./state";
import { createSession } from "./manager";

export function setupKeybinds(renderer: any): void {
  renderer.keyInput.on("keypress", (keyEvent: any) => {
    const key = keyEvent.name;
    const ctrl = keyEvent.ctrl || false;

    // 创建模态打开时，所有键优先给模态
    if (state.createModal) {
      if (key === "escape") {
        state.createModal = null;
        rerender();
      } else if (key === "tab") {
        state.createModal.field =
          state.createModal.field === "path" ? "agent" : "path";
        rerender();
      }
      // 其他键由模态内的 Input 处理
      return;
    }

    // 输入框聚焦时，Tab 才能离开
    if (state.focusedPanel === "input") {
      if (key === "escape") {
        state.focusedPanel = "kanban";
        rerender();
      } else if (key === "tab") {
        state.focusedPanel = "kanban";
        rerender();
      }
      // 其他键由 Input 自己处理
      return;
    }

    // 普通模式快捷键
    switch (key) {
      case "q":
        log("q pressed, exiting");
        rerender();
        setTimeout(() => process.exit(0), 50);
        break;
      case "tab":
        focusNext();
        rerender();
        break;
      case "n":
        state.createModal = {
          field: "path",
          path: process.cwd(),
          agent: "build",
        };
        rerender();
        break;
      case "d":
        if (state.selectedSessionId) {
          state.focusMode = !state.focusMode;
          state.focusedPanel = "detail";
          rerender();
        }
        break;
      case "escape":
        state.focusMode = false;
        state.focusedPanel = "kanban";
        rerender();
        break;
      case "return":
        // Enter 选中当前 worker（在 kanban）→ 进入 focus
        if (state.focusedPanel === "kanban" && state.workers.length > 0) {
          const w = state.workers[0]; // TODO: 真正的选中索引
          switchSession(w.session_id);
          state.focusMode = true;
          state.focusedPanel = "detail";
          rerender();
        }
        break;
      case "i":
        // 进入输入模式
        if (state.selectedSessionId) {
          state.focusedPanel = "input";
          rerender();
        }
        break;
    }
  });
}

function focusNext() {
  state.focusedPanel = nextPanel(state.focusedPanel);
}

function nextPanel(p: string): any {
  if (state.focusMode) {
    return p === "detail" ? "input" : "detail";
  }
  const cycle = ["tree", "kanban", "detail", "input"];
  const i = cycle.indexOf(p);
  return cycle[(i + 1) % cycle.length];
}
