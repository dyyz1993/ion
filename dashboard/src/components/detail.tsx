import React from "react";
import { Box, Text } from "ink";
import { colors, statusIcon } from "../theme";
import { S } from "../state";

export function Detail({ renderer, focusMode }: { renderer: any; focusMode: boolean }) {
  const focused = S.focusedPanel === "detail";
  const bc = focused ? colors.borderFocused : colors.borderInactive;
  const worker = S.workers.find((w) => w.session_id === S.selectedSessionId);

  return (
    <Box borderStyle="round" borderColor={bc} flexDirection="column" paddingX={1} flexGrow={1}>
      <Text color={colors.accent} bold>{" Detail "}</Text>
      {!worker ? (
        <Text color={colors.subtext}>
          Select a worker card{"\n"}(Tab to kanban, Enter to select)
        </Text>
      ) : focusMode ? (
        <FocusMode worker={worker} />
      ) : (
        <CompactMode worker={worker} />
      )}

      {/* 日志 - 最多 3 条 */}
      <Text color={colors.accent} bold>{"\n"}▸ Logs</Text>
      {S.logs.slice(0, 3).map((l, i) => (
        <Text key={i} color={colors.subtext}>{l.slice(0, 60)}</Text>
      ))}
    </Box>
  );
}

function CompactMode({ worker }: { worker: any }) {
  const si = statusIcon(worker.status);
  const log = worker.log_short || "(no output)";
  return (
    <>
      <Text color={si.color} bold>{si.icon} {worker.agent}</Text>
      <Text color={colors.subtext}>{worker.model || "?"}</Text>
      <Text color={colors.text}>▸ {log.slice(0, 50)}</Text>
    </>
  );
}

function FocusMode({ worker }: { worker: any }) {
  const si = statusIcon(worker.status);
  const uptime = fmtUptime(worker.started_at);
  const msgs = S.messages.get(worker.session_id) || [];

  return (
    <>
      <Text color={si.color} bold>{si.icon} {worker.agent}</Text>
      <Text color={colors.subtext}>{uptime} · {worker.status.toUpperCase()}</Text>
      <Text color={colors.subtext}>Project: {worker.project}</Text>
      <Text>{""}</Text>

      {/* 聊天历史 - 最多 6 条 */}
      <Text color={colors.accent} bold>▸ Chat</Text>
      {msgs.length === 0 && (
        <Text color={colors.subtext}>Type a message below and press Enter</Text>
      )}
      {msgs.slice(-6).map((m: any, i: number) => {
        const prefix = m.role === "user" ? "你: " : "AI: ";
        const color = m.role === "user" ? colors.accent : colors.text;
        const lines = m.content.split("\n").slice(0, 4);
        return (
          <Box key={i} flexDirection="column">
            {lines.map((line: string, j: number) => (
              <Text key={j} color={color}>
                {j === 0 ? prefix : "    "}{line.slice(0, 70)}
              </Text>
            ))}
            {m.streaming && <Text color={colors.accent}>▍</Text>}
          </Box>
        );
      })}
    </>
  );
}

function fmtUptime(ts?: number): string {
  if (!ts) return "--:--:--";
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  return `${String(h).padStart(2,"0")}:${String(m).padStart(2,"0")}:${String(sec).padStart(2,"0")}`;
}
