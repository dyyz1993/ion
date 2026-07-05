import React from "react";
import { Box, Text } from "ink";
import { colors, statusIcon } from "../theme";
import { S } from "../state";

export function Tree() {
  const focused = S.focusedPanel === "tree";
  const bc = focused ? colors.borderFocused : colors.borderInactive;

  return (
    <Box borderStyle="round" borderColor={bc} flexDirection="column" padding={1} flexGrow={1}>
      <Text color={colors.accent} bold> Projects · {S.projects.length} </Text>
      {S.projects.length === 0 && <Text color={colors.subtext}>(no projects)</Text>}
      {S.projects.map((p) => (
        <Box key={p.name} flexDirection="column">
          <Text color={colors.text}>▼ {p.name} ({p.worker_count})</Text>
          {S.workers.filter((w) => w.project === p.name).map((w) => {
            const si = statusIcon(w.status);
            return (
              <Text key={w.session_id} color={si.color}>
                {"  "}{si.icon} {w.session_id.slice(0, 8)} {w.agent}
              </Text>
            );
          })}
        </Box>
      ))}
    </Box>
  );
}
