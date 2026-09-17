#!/bin/sh
# approval_push_bridge.sh — 一行包装：转发所有参数给 python3 桥脚本。
# 用法与部署方法见 approval_push_bridge.py 头部注释（nohup + ION_APPROVAL_WEBHOOK /
# ION_APPROVAL_BRIDGE_LOG 两个环境变量；--test-push / --with-url / --selftest）。
exec python3 "$(dirname "$0")/approval_push_bridge.py" "$@"
