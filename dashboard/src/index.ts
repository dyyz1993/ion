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
import { state, setRerenderFn, rerender } from "./state";
import { renderRoot } from "./layout/root";
import { setupKeybinds } from "./keybinds";

async function main() {
  process.on("uncaughtException", (e) => {
    process.stderr.write(`[uncaught] ${e.stack || e}\n`);
    process.exit(1);
  });
  process.on("unhandledRejection", (e) => {
    process.stderr.write(`[unhandled] ${e}\n`);
  });

  const renderer = await createCliRenderer({
    exitOnCtrlC: true,
    targetFps: 30,
  });

  // 注册 rerender：销毁旧树重建
  // OpenTUI 的 root 是个 Renderable，不是 VNode。
  // 销毁旧子树用 getChildren() + destroyRecursively()
  setRerenderFn(() => {
    try {
      // 销毁所有现有子节点
      const children = renderer.root.getChildren();
      for (const child of children) {
        if (child && typeof child.destroyRecursively === "function") {
          child.destroyRecursively();
        } else if (child && typeof renderer.root.remove === "function") {
          renderer.root.remove(child);
        }
      }
      const tree = renderRoot(renderer);
      if (tree) renderer.root.add(tree);
    } catch (e: any) {
      process.stderr.write(`[rerender] ${e.stack || e}\n`);
    }
  });

  // 键盘
  setupKeybinds(renderer);

  // 初次渲染（空数据）
  rerender();
  process.stderr.write("[ion] renderer ready\n");

  // 首次拉取
  try {
    const overview = await pollOverview();
    state.workers = overview.workers || [];
    state.projects = overview.projects || [];
    state.totalStale = overview.total_stale || 0;
    state.connected = true;
    rerender();
    process.stderr.write(`[ion] loaded ${state.workers.length} workers\n`);
  } catch (e: any) {
    process.stderr.write(`[ion] connect failed: ${e.message}\n`);
    state.connected = false;
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
        rerender();
      }
    }
  }, 1000);
}

main().catch((e) => {
  process.stderr.write(`[fatal] ${e.stack || e}\n`);
  process.exit(1);
});
