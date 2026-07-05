import React from "react";
import { Box, Text } from "ink";
import { colors } from "../theme";
import { S } from "../state";

export function StatusBar() {
  const live = S.workers.filter((w) => w.status !== "dead").length;
  const connColor = S.connected ? colors.accent : colors.dead;
  const connIcon = S.connected ? "●" : "○";

  return (
    <Box>
      <Text color={connColor}>{connIcon} connected</Text>
      <Text color={colors.subtext}>
        {" "}{S.workers.length} workers | {live} live
        {" | stale: "}{S.totalStale}
        {" | "}{S.focusedPanel}{"  "}
        Tab:switch Enter:select n:new i:input d:focus q:quit
      </Text>
    </Box>
  );
}
