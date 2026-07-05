/**
 * ION Dashboard - Ink App
 *
 * 所有键盘处理在 useInput 里。Input 字段用纯键盘驱动。
 */
import React, { useRef, useEffect, useState, useMemo } from "react";
import { Box, Text, useInput, useApp } from "ink";
import { colors } from "./theme";
import { S, useRefresh, refresh, log } from "./state";
import { pollOverview, createSession, sendPrompt } from "./manager";
import { Tree } from "./components/tree";
import { Kanban } from "./components/kanban";
import { Detail } from "./components/detail";
import { StatusBar } from "./components/status_bar";

export function App() {
  useRefresh();

  // ── 键盘 ──
  useInput((input, key) => {
    // q 退出
    if (input === "q") process.exit(0);

    // 创建模态
    if (S.createModal) {
      if (key.escape) { S.createModal = null; refresh(); return; }
      if (key.return) {
        const m = S.createModal;
        if (!m.path) { m.error = "路径不能为空"; refresh(); return; }
        createSession(m.path, m.agent)
          .then(() => { S.createModal = null; refresh(); })
          .catch((e: any) => { m.error = e.message; refresh(); });
        return;
      }
      if (key.tab) {
        S.createModal.field = S.createModal.field === "path" ? "agent" : "path";
        refresh(); return;
      }
      if (key.backspace || key.delete) {
        if (S.createModal.field === "path") S.createModal.path = S.createModal.path.slice(0, -1);
        else S.createModal.agent = S.createModal.agent.slice(0, -1);
        refresh(); return;
      }
      if (input && input.length === 1) {
        const m = S.createModal;
        const isDef = m.field === "agent" && ["build","explore","plan","reviewer"].includes(m.agent);
        if (isDef) m.agent = "";
        if (m.field === "path") m.path += input; else m.agent += input;
        refresh();
      }
      return;
    }

    // 输入模式
    if (S.focusedPanel === "input") {
      if (key.return) {
        const t = S.inputValue.trim();
        if (t && S.selectedSessionId) {
          const msgs = S.messages.get(S.selectedSessionId) || [];
          msgs.push({ role: "user", content: t });
          S.messages.set(S.selectedSessionId, msgs);
          sendPrompt(S.selectedSessionId, t).catch(() => {});
          S.inputValue = "";
          refresh();
        }
        return;
      }
      if (key.escape || key.tab) { S.focusedPanel = "kanban"; refresh(); return; }
      if (key.backspace || key.delete) { S.inputValue = S.inputValue.slice(0, -1); refresh(); return; }
      if (input && input.length === 1) { S.inputValue += input; refresh(); return; }
      return;
    }

    // 普通模式
    if (key.tab) {
      const cyc = ["tree", "kanban", "detail"];
      S.focusedPanel = cyc[(cyc.indexOf(S.focusedPanel) + 1) % 3];
      refresh(); return;
    }
    if (key.return && S.focusedPanel === "kanban" && S.workers.length > 0) {
      S.selectedSessionId = S.workers[0].session_id;
      S.focusMode = true;
      S.focusedPanel = "detail";
      refresh(); return;
    }
    if (input === "n") {
      S.createModal = { field: "path", path: process.cwd(), agent: "build" };
      refresh(); return;
    }
    if (input === "i") { S.focusedPanel = "input"; refresh(); return; }
    if (input === "d" && S.selectedSessionId) {
      S.focusMode = !S.focusMode;
      S.focusedPanel = "detail";
      refresh(); return;
    }
    if (key.escape) {
      S.focusMode = false;
      S.focusedPanel = "kanban";
      refresh();
    }
  });

  // ── 首次拉取 + 轮询 ──
  useEffect(() => {
    const load = async () => {
      try {
        const ov = await pollOverview();
        S.workers = ov.workers || [];
        S.projects = ov.projects || [];
        S.totalStale = ov.total_stale || 0;
        S.connected = true;
        log(`loaded ${S.workers.length} workers`);
        refresh();
      } catch (e: any) {
        S.connected = false;
        log(`connect: ${e.message}`);
        refresh();
      }
    };
    load();
    const iv = setInterval(async () => {
      try {
        const ov = await pollOverview();
        S.workers = ov.workers || [];
        S.projects = ov.projects || [];
        S.totalStale = ov.total_stale || 0;
        S.connected = true;
        refresh();
      } catch { if (S.connected) { S.connected = false; log("disconnected"); refresh(); } }
    }, 1000);
    return () => clearInterval(iv);
  }, []);

  // ── 渲染 ──
  const theme = colors;
  const focusMode = S.focusMode && S.selectedSessionId;

  return (
    <Box flexDirection="column">
      {/* 主区域 */}
      <Box flexGrow={1}>
        {focusMode ? (
          <Box flexGrow={4}>
            <Detail renderer={null as any} focusMode={true} />
          </Box>
        ) : (
          <>
            <Box flexGrow={1} minWidth={20}>
              <Tree />
            </Box>
            <Box flexGrow={3}>
              <Kanban />
            </Box>
            <Box flexGrow={1} minWidth={25}>
              <Detail renderer={null as any} focusMode={false} />
            </Box>
          </>
        )}
      </Box>

      {/* 输入框 */}
      <InputBar />

      {/* 状态栏 */}
      <StatusBar />

      {/* 创建模态（浮层） */}
      {S.createModal && <CreateModal />}
    </Box>
  );
}

// ── 输入框组件 ──
function InputBar() {
  const focused = S.focusedPanel === "input";
  const borderColor = focused ? colors.borderFocused : colors.borderInactive;
  const t = S.inputValue || "Type a message and press Enter... (i to focus)";
  const fg = S.inputValue ? colors.text : colors.subtext;

  return (
    <Box borderStyle="round" borderColor={borderColor} paddingX={1} height={3}>
      <Text color={fg}>{t}</Text>
    </Box>
  );
}

// ── 创建模态 ──
function CreateModal() {
  if (!S.createModal) return null;
  const m = S.createModal;
  return (
    <Box flexDirection="column" marginTop={5} width={60}
      borderStyle="round" borderColor={colors.accent} padding={1}>
      <Text color={colors.accent} bold>✦ Create New Worker</Text>
      <Text color={colors.subtext}>Worker 会在指定项目目录下工作</Text>
      <Box borderStyle="round" borderColor={m.field === "path" ? colors.accent : colors.borderInactive}>
        <Text color={colors.text}> {m.path}</Text>
      </Box>
      <Box borderStyle="round" borderColor={m.field === "agent" ? colors.accent : colors.borderInactive}>
        <Text color={colors.text}> {m.agent}</Text>
      </Box>
      <Text color={m.error ? colors.warning : colors.subtext}>
        Tab 切字段  Enter 创建  Esc 取消{m.error ? `  ⚠ ${m.error}` : ""}
      </Text>
    </Box>
  );
}
