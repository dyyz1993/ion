# CI Matrix Report — Final

> Generated: 2026-07-27
> Run: `PARALLELISM=5 PER_SCRIPT_TIMEOUT=300 bash scripts/run_ci_matrix_parallel.sh`
> Commits: 5ef7b0a → 14e1ddb (4 bug fixes)

## Summary

| Status | Count | % |
|--------|-------|---|
| ✅ PASS | 15 | 27% |
| ❌ FAIL (timeout) | 13 | 24% |
| ❌ FAIL (exit≠0) | 21 | 38% |
| ⏭️ SKIP | 6 | 11% |
| **Total** | **55** | |

## ✅ PASS (15) — verified green

| Script | Duration | Notes |
|--------|----------|-------|
| cli_alignment_ci | 35s | |
| crash_recovery_ci | 1s | |
| export_ci | 4s | |
| extension_cli_ci | 1s | |
| extension_flags_ci | 3s | Fixed by serve spawn fix |
| extension_fs_ci | 177s | Fixed by serve spawn fix |
| faux_scenarios_ci | 40s | |
| hooks_ci | 6s | |
| hooks_handler_ci | 71s | Fixed by serve spawn fix |
| memory_agent_ci | 8s | Fixed by serve spawn fix |
| memory_v2_processing_ci | 6s | Fixed by serve spawn fix |
| message_source_ci | 11s | Fixed by ION_SESSION_DIR snippet |
| record_replay_ci | 9s | |
| session_tree_ci | 1s | |
| sessions_ci | 7s | |

## ❌ FAIL — categorized by root cause

### Category 1: Parallel Race (8 scripts)
These scripts pass when run alone but fail in parallel. Root cause: 5 scripts
each start their own `ion serve` simultaneously, and the memory-agent singleton
 + monitor singleton in each host compete for resources (CPU, LLM API rate limits).

**Fix**: reduce PARALLELISM to 2-3, or disable singletons in test mode.

- extensions_ci, p3_ui_ci, p4_events_ci, permission_store_ci
- realtime_stitch_ci, rollback_impact_ci, session_hook_ci, tier_models_ci

### Category 2: Test Logic Failures (10 scripts)
Real test assertion failures — need individual investigation.

- abort_ci (1/5): abort event timing
- global_memory_ci (1/8): save timing
- hooks_agent_ci (1/4): worker count boundary
- memory_active_ci (1/25): importance sort
- memory_injection_ci (5/7): save/search/forget chain
- p2_hotreload_ci (2/9): list_rules + reload
- p3_audit_ci (3/7): audit log format
- skill_tool_ci (7/27): skill list/inject/fork
- soft_interrupt_ci (2/3): interrupt latency + steer
- sse_events_ci (8/8): event subscription (known faux error-injection issue)

### Category 3: Timeout 300s (13 scripts)
Scripts that internally run long `ion serve` + multi-round RPC + sleep patterns.
Need 600s+ timeout or test refactoring.

- compaction, file_snapshot, lsp, message_retrieval, monitor
- overflow_recovery, permission, runtime, scenario2
- session_entries, soft_delete, streaming_throughput
- team_e2e, ui_integration, workflow

## Bugs Found & Fixed (this session)

| Bug | Issue | Commit | Impact |
|-----|-------|--------|--------|
| Events sessionId="" | #29 | 5ef7b0a | All subscribe --session broken |
| Rollback save dedup | #28 | e76e1be | Multi-turn rollback loses messages |
| FauxProvider no repeat | #28 | e76e1be | Multi-turn tests fail on round 2 |
| Session isolation missing | #30 | 5e340ec + 9ae92a3 | Parallel scripts collide |
| **serve blocks socket loop** | — | **14e1ddb** | **All serve-based RPC timeout** |

The serve-blocking bug (14e1ddb) was the highest-impact fix: it was the root
cause of 8+ "create_session failed" / "host not responding" failures.

## Recommendations

1. **Reduce PARALLELISM to 3** for scripts that use `ion serve` (they're heavy)
2. **Disable singletons in test mode** (ION_DISABLE_SINGLETONS=1 env var)
3. **Investigate the 10 test_logic failures** individually (some may be real bugs)
4. **Increase timeout to 600s** for the 13 slow scripts
