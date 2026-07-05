import React from "react";
import { Box, Text } from "ink";
import { colors, statusIcon } from "../theme";
import { S } from "../state";

export function Kanban() {
  const focused = S.focusedPanel === "kanban";
  const bc = focused ? colors.borderFocused : colors.borderInactive;

  return (
    <Box borderStyle="round" borderColor={bc} flexDirection="column" padding={1}>
      <Text color={colors.accent} bold> Workers · {S.workers.length} </Text>
      {S.workers.length === 0 && (
        <Text color={colors.subtext}>No active workers{'\n'}Press 'n' to create one</Text>
      )}
      {S.workers.map((w) => {
        const si = statusIcon(w.status);
        const uptime = fmtUptime(w.started_at);
        const log = (w.log_short || "(no output)").slice(0, 50);
        return (
          <Box key={w.session_id} borderStyle="round" borderColor={colors.borderNormal} padding={1} flexDirection="column" marginBottom={1}>
            <Text color={si.color} bold>{si.icon} {w.agent}</Text>
            <Text color={colors.subtext}>{uptime} · {w.status.toUpperCase()}</Text>
            <Text color={colors.subtext}>{w.model || "?"}</Text>
            <Text color={colors.text}>▸ {log}</Text>
          </Box>
        );
      })}
    </Box>
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
