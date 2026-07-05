import React from "react";
import { Box, Text } from "ink";
import { colors, statusIcon } from "../theme";
import { S } from "../state";

export function Kanban() {
  const focused = S.focusedPanel === "kanban";
  const bc = focused ? colors.borderFocused : colors.borderInactive;
  const maxHeight = (process.stdout.rows || 40) - 6; // 留出行给 tree/detail/input/status
  const maxCards = Math.min(S.workers.length, Math.floor(maxHeight / 4));

  return (
    <Box borderStyle="round" borderColor={bc} flexDirection="column" paddingX={1}>
      <Text color={colors.accent} bold> Workers · {S.workers.length} </Text>
      {S.workers.length === 0 && (
        <Text color={colors.subtext}>No active workers{"\n"}Press 'n' to create one</Text>
      )}
      {S.workers.slice(0, maxCards).map((w) => {
        const si = statusIcon(w.status);
        const uptime = fmtUptime(w.started_at);
        const log = (w.log_short || "").slice(0, 40);
        return (
          <Box key={w.session_id} flexDirection="column">
            <Text color={si.color} bold>{si.icon} {w.agent}</Text>
            <Text color={colors.subtext}>{uptime} {w.status.toUpperCase()} {w.model || "?"}</Text>
            {log && <Text color={colors.text}>▸ {log}</Text>}
          </Box>
        );
      })}
      {S.workers.length > maxCards && (
        <Text color={colors.subtext}>... and {S.workers.length - maxCards} more</Text>
      )}
    </Box>
  );
}

function fmtUptime(ts?: number): string {
  if (!ts) return "--:--:--";
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  return `${String(Math.floor(s / 3600)).padStart(2,"0")}:${String(Math.floor((s % 3600) / 60)).padStart(2,"0")}:${String(s % 60).padStart(2,"0")}`;
}
