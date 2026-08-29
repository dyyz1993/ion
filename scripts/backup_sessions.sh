#!/bin/bash
# 会话备份：最近 48h 内有变动的会话文件快照到 sessions-backup/（保留 7 天）
# 用法：bash scripts/backup_sessions.sh（可挂 cron/lunchd，或重要操作前手动跑）
set -u
SRC="$HOME/.ion/agent/sessions"
DST="$HOME/.ion/agent/sessions-backup/$(date +%Y%m%d_%H%M)"
mkdir -p "$DST"
find "$SRC" -name "*.jsonl" -mmin -2880 -exec cp {} "$DST/" \; 2>/dev/null
# 清 7 天前的备份
find "$HOME/.ion/agent/sessions-backup" -maxdepth 1 -type d -mtime +7 -exec rm -rf {} + 2>/dev/null
echo "备份完成 → $DST ($(ls "$DST" | wc -l | tr -d ' ') 个文件)"
