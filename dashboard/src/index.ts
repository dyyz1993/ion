/**
 * ION Dashboard —— OpenTUI 入口
 *
 * 启动流程：
 * 1. createCliRenderer
 * 2. 注册 rerender 函数（重建整树）
 * 3. 注册键盘快捷键
 * 4. setInterval 每秒 poll get_overview
 */
import { createCliRenderer, Box, Text } from "@opentui/core";
import { pollOverview } from "./manager";
import { state, setRerenderFn, rerender, log } from "./state";
import { renderRoot } from "./layout/root";
import { setupKeybinds } from "./keybinds";

async function main() {
  process.on("uncaughtException", (e: any) => {
    log(`uncaught: ${e.message}`);
  });
  process.on("unhandledRejection", (e: any) => {
    log(`unhandled: ${e}`);
  });

  const renderer = await createCliRenderer({
    exitOnCtrlC: true,
    targetFps: 30,
  });

  setRerenderFn(() => {
    try {
      const children = renderer.root.getChildren();
      for (const child of children) {
        if (child && typeof child.destroyRecursively === "function") {
          child.destroyRecursively();
        }
      }
      const tree = renderRoot(renderer);
      if (tree) renderer.root.add(tree);
    } catch (e: any) {
      // 错误塞进 state.logs，不污染终端
      log(`rerender error: ${e.message}`);
    }
  });

  setupKeybinds(renderer);
  rerender();
  log("renderer ready");

  // 首次拉取
  try {
    const overview = await pollOverview();
    state.workers = overview.workers || [];
    state.projects = overview.projects || [];
    state.totalStale = overview.total_stale || 0;
    state.connected = true;
    log(`loaded ${state.workers.length} workers`);
    rerender();
  } catch (e: any) {
    state.connected = false;
    log(`connect failed: ${e.message}`);
    rerender();
  }

  // 轮询 Manager（每秒一次）
  setInterval(async () => {
    try {
      const overview = await pollOverview();
      state.workers = overview.workers || [];
      state.projects = overview.projects || [];
      state.totalStale = overview.total_stale || 0;
      state.connected = true;
      rerender();
    } catch (e) {
      if (state.connected) {
        state.connected = false;
        log("disconnected from manager");
        rerender();
      }
    }
  }, 1000);
}

main().catch((e) => {
  // 致命错误才写 stderr
  process.stderr.write(`[fatal] ${e.stack || e}\n`);
  process.exit(1);
});
