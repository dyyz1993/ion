#!/usr/bin/env bash
# ext_full_cases.sh — 全扩展测试 case 清单（可 bash source）
#
# 总计：227 个 case，按 23 个扩展分组（GlobalMemoryStore 已合并入 global-memory）
#
# 字段格式（用 | 分隔，每行一个 case）：
#   CASE_ID|EXT_NAME|SETUP_FN|PROMPT
#
#   CASE_ID     形如 GM-01（组内唯一、递增）
#   EXT_NAME    扩展名（与 src/ 下的模块对应）
#   SETUP_FN    bash 函数名（需要预准备的 case 才填，否则留空）
#   PROMPT      1-2 句话的自然语言提示，足以触发对应扩展的核心行为
#
# 用法（在测试脚本中）：
#   source "$ROOT/scripts/ext_full_cases.sh"
#   for c in "${EXT_FULL_CASES[@]}"; do
#       IFS='|' read -r id ext setup prompt <<< "$c"
#       ...
#   done

# ── EXT: global-memory (20 cases) ────────────────────────────────────────
GM_CASES=(
    "GM-01|global-memory||用 memory_save 保存一条带 tags=[rust,async] 和 category=best-practice 的记忆，内容是关于 tokio::spawn 需要 Send bound。"
    "GM-02|global-memory||用 memory_search 搜 'tokio' 关键字，验证能命中刚保存的记忆，并报告 score。"
    "GM-03|global-memory||保存一条 memory 后用 memory_forget 删掉，然后再 search 应该返回空。"
    "GM-04|global-memory||保存 3 条 memory，再用 memory_search global=true 跨 project 搜索，确认能返回其它 project 的同主题条目。"
    "GM-05|global-memory||用 memory_save 保存 tags 为空数组、category 为 'general' 的 memory，验证不会报错。"
    "GM-06|global-memory||用 memory_save 故意不传 content 参数（构造非法输入），验证扩展返回明确错误。"
    "GM-07|global-memory||保存一条超长 content（重复某字符 5000 次），验证截断或警告行为。"
    "GM-08|global-memory||连续并发触发 5 次 memory_save（不同 tags），最后 search 确认全部 5 条都成功落盘。"
    "GM-09|global-memory||用 memory_search 搜一个完全不可能存在的关键字 'zzqqxxww'，确认返回空数组而非报错。"
    "GM-10|global-memory||用 memory_search 搜空字符串，验证扩展对空 query 的容错。"
    "GM-11|global-memory||保存一条带多 tag 的 memory（tags 含中文、emoji），验证 unicode 标签正常落盘并能搜到。"
    "GM-12|global-memory||保存两条内容相似（仅 1 字之差）的 memory，再 search 同关键字，验证相似度排序合理。"
    "GM-13|global-memory||保存后立即修改系统时钟（用 faketime 思路：仅描述），验证 created_at 字段格式正确。"
    "GM-14|global-memory||连续保存 50 条 memory 后 search，验证性能无明显退化（< 500ms）。"
    "GM-15|global-memory||用 memory_search category='warning' 过滤，验证只返回该分类的条目。"
    "GM-16|global-memory||先在 project A 保存一条 memory，切到 project B 后 search global=true 同关键字，验证跨 project 命中。"
    "GM-17|global-memory||保存一条 memory 后立即读取 .ion/memory 存储文件，验证落盘格式（JSON/SQLite）正确。"
    "GM-18|global-memory||保存 3 条 tags 完全不同的 memory，再用 memory_search 仅按 tag 过滤（无关键字），验证按 tag 检索。"
    "GM-19|global-memory||保存一条 memory，再用 memory_forget 传一个不存在的 id，验证返回明确错误而非崩溃。"
    "GM-20|global-memory||保存一条 memory 时附带 source='test-case' 元数据，再 search 验证元数据被保留。"
)

# ── EXT: file_snapshot (19 cases) ─────────────────────────────────────────
FS_CASES=(
    "FS-01|file_snapshot||用 write 创建 /tmp/fs_test/lib.rs，验证 file_snapshot 自动生成一条 snapshot。"
    "FS-02|file_snapshot||用 write 覆盖已有文件 3 次（每次改 1 行），用 snapshot_diff 查看相邻 snapshot 的差异。"
    "FS-03|file_snapshot||用 write 在 /tmp/fs_multi/ 下同时创建 5 个不同文件，验证每个文件都生成独立 snapshot 链。"
    "FS-04|file_snapshot||用 write 写一个 100KB 大文件，再覆盖一个 100KB 版本，验证 snapshot_diff 能正确处理大文件 diff。"
    "FS-05|file_snapshot|setup_fs_binary|用 write 尝试写一个二进制内容（含 \\x00 字节）的文件，验证 snapshot 不崩溃且 hash 正确。"
    "FS-06|file_snapshot||用 write 创建 a.txt，再用 snapshot_restore 回滚到空状态（删除前），验证文件被恢复。"
    "FS-07|file_snapshot||连续覆盖同一文件 10 次，触发 snapshot GC，验证旧 snapshot 被清理（保留最近 N 条）。"
    "FS-08|file_snapshot||用 write 创建嵌套深路径 a/b/c/d.txt，验证 snapshot 正确记录并按路径索引。"
    "FS-09|file_snapshot||用 write 创建目录下多文件后用 bash rm 删除其中一个，验证 snapshot_diff 能反映删除事件。"
    "FS-10|file_snapshot||用 write 重命名文件（旧路径写空 + 新路径写内容）后，验证 snapshot 链不串号。"
    "FS-11|file_snapshot||快速连续 write 同一文件 50 次（同一 tick），验证 snapshot 去重或合并策略生效。"
    "FS-12|file_snapshot||用 snapshot_restore 回滚到一个不存在的 snapshot id，验证返回明确错误。"
    "FS-13|file_snapshot||用 write 写一个文件，kill 进程后重启 session，验证 snapshot 历史仍然可读。"
    "FS-14|file_snapshot||用 write 在 symlink 路径上写文件，验证 snapshot 解析真实路径而非 link 路径。"
    "FS-15|file_snapshot||用 write 写一个只读权限文件，再尝试覆盖，验证 snapshot 在权限拒绝时的降级行为。"
    "FS-16|file_snapshot||同时 write 3 个文件 + 覆盖 2 个，用 snapshot_list 查看所有受跟踪文件的当前状态。"
    "FS-17|file_snapshot||用 write 创建文件后立即 snapshot_diff 同一文件（相同 hash），验证返回空 diff。"
    "FS-18|file_snapshot||在 .gitignore 中加入路径后 write 该路径，验证 snapshot 是否仍然记录（按设计确认行为）。"
    "FS-19|file_snapshot||用 write 写一个含 BOM 的 UTF-8 文件，验证 snapshot_diff 的行级 diff 正确（不被 BOM 干扰）。"
)

# ── EXT: hooks (17 cases) ─────────────────────────────────────────────────
HK_CASES=(
    "HK-01|hooks|setup_hooks_postuse|配置 hooks.json 让 PostToolUse 在 write 后触发 echo 'post' >> /tmp/hook.log，然后 write 一个文件，验证日志被写入。"
    "HK-02|hooks|setup_hooks_stop|配置 Stop hook 触发 echo 'stop' >> /tmp/hook.log，让 agent 完成一轮回答，验证 stop 事件被触发。"
    "HK-03|hooks|setup_hooks_sessionstart|配置 SessionStart hook 写入 /tmp/hook.log，开启新 session，验证日志在 session 启动时被写入。"
    "HK-04|hooks|setup_hooks_preblock|配置 PreToolUse hook 对 bash 命令返回 block，然后让 agent 跑 bash，验证命令被拦截。"
    "HK-05|hooks|setup_hooks_matcher|配置 PostToolUse hook matcher=write，触发 read 工具，验证 hook 不被错误触发（matcher 生效）。"
    "HK-06|hooks|setup_hooks_cond|配置带 if 条件的 hook（仅当文件路径含 .lock 时触发），write 一个 .lock 文件和一个普通文件，验证条件分支。"
    "HK-07|hooks|setup_hooks_async|配置 async=true 的 PostToolUse hook（耗时 2s），触发后验证主流程不被阻塞。"
    "HK-08|hooks|setup_hooks_rewake|配置 hook 输出包含 rewake=true，触发后验证 agent 被重新唤醒继续下一轮。"
    "HK-09|hooks|setup_hooks_disable|配置 hooks.json disableAll=true，触发任意工具，验证所有 hook 都不执行。"
    "HK-10|hooks|setup_hooks_once|配置 once=true 的 hook，连续触发 3 次同事件，验证 hook 只执行 1 次。"
    "HK-11|hooks|setup_hooks_looplimit|配置 loop_limit=2 的 hook 配合 rewake，验证第 3 次后自动停止循环。"
    "HK-12|hooks|setup_hooks_multi|配置同事件 2 个 hook（A 写日志、B 写另一个日志），触发后验证两个 hook 都执行（顺序不重要）。"
    "HK-13|hooks|setup_hooks_fail|配置一个注定失败的 hook（exit 1），触发后验证主流程不被中断且错误被记录。"
    "HK-14|hooks|setup_hooks_env|配置 hook 读取 ION_SESSION_ID 环境变量，触发后验证 hook 能拿到正确 session id。"
    "HK-15|hooks|setup_hooks_timeout|配置 timeout=100ms 的 hook（实际 sleep 1s），触发后验证 hook 被超时杀掉。"
    "HK-16|hooks|setup_hooks_precontext|配置 PreToolUse hook 注入额外 context（输出 'INJECT-MARKER'），验证下一轮 prompt 包含该 marker。"
    "HK-17|hooks|setup_hooks_reload|配置一个 hook 后修改 hooks.json，验证扩展能热重载新规则。"
)

# ── EXT: goal_supervisor (16 cases) ───────────────────────────────────────
GS_CASES=(
    "GS-01|goal_supervisor||用 goal_set 设定一个目标：'在 /tmp/gs_test 写一个 hello.rs 并 cargo run'，objective=high，验证返回 goal_id。"
    "GS-02|goal_supervisor||设定一个 goal，用 goal_diagnose 查看当前进度状态（checks 列表、距离完成多远）。"
    "GS-03|goal_supervisor||设定一个带 3 个 checks 的 goal，逐个完成 checks，验证 goal_diagnose 在全部 done 后状态变为 completed。"
    "GS-04|goal_supervisor||设定一个 max_iter=5 的 goal，故意走 6 轮循环，验证 guard 在第 5 轮后强制终止。"
    "GS-05|goal_supervisor||设定一个相似度阈值=0.9 的 goal，给出与 objective 完全不相关的输出，验证 supervisor 标记为 drifted。"
    "GS-06|goal_supervisor||设定一个 timeout=30s 的 goal，故意拖延 60s 才完成，验证 timeout 触发失败。"
    "GS-07|goal_supervisor||设定一个 cost 上限的 goal，触发高 cost 调用后，验证 cost_guard 拦截后续调用。"
    "GS-08|goal_supervisor||设定一个 goal，跑偏后用 goal_refine 调整 objective，验证 supervisor 接受新方向。"
    "GS-09|goal_supervisor||设定一个 goal 后用 goal_rollback 回滚到上一个 checkpoint，验证状态被正确恢复。"
    "GS-10|goal_supervisor||设定一个 goal 中 checks 包含 'file_exists /tmp/x'，write 文件后 diagnose 应反映 check 已通过。"
    "GS-11|goal_supervisor||设定两个并行 goal，验证 goal_id 隔离正确，diagnose 不互相串。"
    "GS-12|goal_supervisor||设定一个 goal，触发 guard 后查看诊断信息中的 'failure_reason' 字段格式。"
    "GS-13|goal_supervisor||设定一个空 checks 的 goal，验证扩展要么报错要么标记为待补充。"
    "GS-14|goal_supervisor||设定一个 objective 极长（5000 字）的 goal，验证存储和检索不被截断。"
    "GS-15|goal_supervisor||设定一个 goal 后用 bash 删除其产物文件，diagnose 应反映 check 重新失败。"
    "GS-16|goal_supervisor||设定 goal 完成后继续 diagnose，验证状态稳定为 completed（不会反复变更）。"
)

# ── EXT: MonitorExtension (13 cases) ──────────────────────────────────────
MN_CASES=(
    "MN-01|monitor||用 monitor 注册一个指标 'disk_usage'，触发一次采样，验证指标被记录。"
    "MN-02|monitor||通过 event_bus 发布一个 'test-event' 事件，用 monitor 监听后验证事件被消费。"
    "MN-03|monitor||注册 3 个不同指标后启动 monitor，验证 monitor_cooldown 节流策略生效。"
    "MN-04|monitor||触发一个 monitor 触发条件（如 disk>80%），验证 monitor_triggered 事件被发出。"
    "MN-05|monitor||连续触发 5 次同 monitor（在 cooldown 内），验证 monitor_throttled 计数增加而非重复触发。"
    "MN-06|monitor||用 monitor 启动一个后台采样任务，session 结束后验证 monitor_spawned 计数正确。"
    "MN-07|monitor||暂停一个 monitor 后再触发其条件，验证 monitor_skipped 而非 triggered。"
    "MN-08|monitor||恢复暂停的 monitor，再触发条件，验证 monitor_triggered 恢复。"
    "MN-09|monitor||触发 monitor 后让它进入 monitor_queued 队列，验证队列长度可查询。"
    "MN-10|monitor||配置 monitor 在事件 bus 上订阅特定 topic，发布不相关 topic 时验证不触发。"
    "MN-11|monitor||并发触发 3 个不同 monitor，验证互不阻塞（独立采样）。"
    "MN-12|monitor||删除一个已注册的 monitor，再触发其条件，验证不再有事件产生。"
    "MN-13|monitor||配置 monitor 在事件 bus 上订阅特定 topic，发布匹配 topic 时验证触发并记录指标。"
)

# ── EXT: bash (11 cases) ──────────────────────────────────────────────────
BS_CASES=(
    "BS-01|bash||用 bash 跑 echo hello，验证前台命令立即返回 stdout='hello'。"
    "BS-02|bash||用 bash background=true 跑 sleep 10 &，验证立即返回 bid，且进程在后台运行。"
    "BS-03|bash||用 bash 跑 sleep 5，配置 timeout=1000ms，验证命令被超时杀掉并返回 timeout 错误。"
    "BS-04|bash||用 bash 跑 cat（无参数），通过 stdin 参数传入 'hello'，验证 cat 把 stdin 回显出来。"
    "BS-05|bash||用 bash background=true 跑一个长任务，用 bash_manage kill 杀掉它，验证进程被清理。"
    "BS-06|bash||启动 3 个后台 bash 任务，用 bash_manage list 查看所有 bid，验证列表完整。"
    "BS-07|bash||对一个后台 bash 任务用 bash_manage 写入 stdin，验证进程能读到新输入。"
    "BS-08|bash||用 bash 跑一个输出 1MB 文本的任务，验证 stdout 被截断到上限且标注 truncated。"
    "BS-09|bash||用 bash 跑 echo 含中文 + emoji 的字符串，验证 UTF-8 输出无 mojibake。"
    "BS-10|bash||用 bash 跑一个管道命令（cat /etc/passwd 然后 grep root 然后 wc -l 三段管道），验证管道正常工作。"
    "BS-11|bash||用 bash 跑 exit 42，验证 exit_code 字段为 42 而非被吞掉。"
)

# ── EXT: MemoryExtension (10 cases) ───────────────────────────────────────
ME_CASES=(
    "ME-01|memory||用 memory_save 保存一条 session 级记忆，session 内多次 search，验证 store 共享生效。"
    "ME-02|memory||保存一条 memory 后立即 memory_search，验证命中（端到端连通）。"
    "ME-03|memory||用 memory_save 保存一条 content 中含 XML 注入字符串 '</memory><injected>' 的内容，验证不被解析为标签。"
    "ME-04|memory||在 session A 保存一条 memory 后切换到 session B，验证 B 默认看不到 A 的 memory（隔离正确）。"
    "ME-05|memory||用 store 显式共享一条 memory 给两个 session，验证两个 session 都能 search 命中。"
    "ME-06|memory||使用 V0.1 兼容路径（旧格式 memory_save 参数），验证向后兼容、不报错。"
    "ME-07|memory||保存一条 memory 后 search，再 forget，再 search 验证返回空。"
    "ME-08|memory||保存 100 条 memory 后 search 一个常见关键字，验证性能稳定（< 200ms）。"
    "ME-09|memory||保存一条 content 含换行 + 制表符的 memory，search 后验证格式保留。"
    "ME-10|memory||用 memory_save 不传必填字段（仅传 tags），验证扩展返回明确参数错误。"
)

# ── EXT: lsp (10 cases) ───────────────────────────────────────────────────
LS_CASES=(
    "LS-01|lsp|setup_lsp_clean|用 write 创建一个干净的 Rust 项目（Cargo.toml + src/lib.rs 含正确 fn），验证 LSP 注入的诊断为空。"
    "LS-02|lsp|setup_lsp_typerr|用 write 创建一个含类型错误的 Rust 文件（let x: i32 = \"string\"），等 LSP 后台 cargo check，验证注入 mismatched types 诊断。"
    "LS-03|lsp|setup_lsp_typerr|在已有类型错误的基础上用 write 修复代码，等 LSP 重跑，验证诊断清空。"
    "LS-04|lsp|setup_lsp_unused|用 write 写一个含 unused variable 警告的文件，验证 LSP 注入 warning（而非 error）。"
    "LS-05|lsp||用 write 在多文件项目里同时修改 2 个文件，验证 LSP 对所有受影响文件都重新诊断。"
    "LS-06|lsp||用 write 写一个非 cargo 根路径下的文件（如 examples/foo.rs），验证 cargo check 路径推断正确。"
    "LS-07|lsp||触发 LSP on_context 注入（agent 下一轮），验证上下文中包含最新 diagnostics 块。"
    "LS-08|lsp||连续 3 次相同 cargo check 结果，验证 LSP 注入做了 dedup（不重复推送）。"
    "LS-09|lsp||故意写一个含 5 个错误的文件，验证 LSP 注入全部 5 条且按行号排序。"
    "LS-10|lsp||在非 Rust 项目目录触发 LSP，验证扩展优雅降级（不崩溃，返回 unsupported）。"
)

# ── EXT: rules-engine (8 cases) ───────────────────────────────────────────
RE_CASES=(
    "RE-01|rules-engine|setup_rules_global|在 ~/.ion/rules.yaml 配置全局规则 'reply in English'，验证 agent 下一轮回复为英文。"
    "RE-02|rules-engine|setup_rules_project|在项目 .ion/rules.yaml 配置规则，验证项目规则被加载且优先级正确。"
    "RE-03|rules-engine||配置一条规则 applyTo: '**/*.rs'，触发非 .rs 文件操作，验证规则不应用。"
    "RE-04|rules-engine|setup_rules_yaml|配置一个 YAML 含多行字符串的规则，验证解析器正确读出全文。"
    "RE-05|rules-engine|setup_rules_global|配置规则后触发一次 agent，验证系统 prompt 中包含规则注入块。"
    "RE-06|rules-engine||session 结束后销毁规则，验证下次 session 默认不加载（除非显式开启）。"
    "RE-07|rules-engine|setup_rules_multi|配置 3 条规则（不同 applyTo），触发不同文件操作，验证规则按 glob 各自匹配。"
    "RE-08|rules-engine||配置一条语法错误的 YAML 规则，验证扩展报错而非崩溃，错误信息含行号。"
)

# ── EXT: learning (8 cases) ───────────────────────────────────────────────
LR_CASES=(
    "LR-01|learning|setup_learning_skill|在一个含 skill 调用的 session 结束后，验证 skill_distillation 自动产出一条 skill 草稿。"
    "LR-02|learning||配置 learning 在 write 触发次数 > 10 时蒸馏，连续 write 11 次后验证触发。"
    "LR-03|learning|setup_learning_sensitive|在一个含敏感信息（API key 形如 sk-xxx）的 session 后蒸馏，验证脱敏后输出不含原文。"
    "LR-04|learning||配置 learning 为 NO_SKILL 模式（关闭蒸馏），跑一个 session，验证不产出任何 skill。"
    "LR-05|learning||在一个跨多文件操作的 session 后蒸馏，验证产出的 skill 涵盖多文件操作模式。"
    "LR-06|learning||在一个空 session（无任何工具调用）后触发蒸馏，验证扩展不产出空 skill。"
    "LR-07|learning||连续两次跑完全相同的 session，验证第二次蒸馏被去重（不重复产出相同 skill）。"
    "LR-08|learning||跑一个含 bash + write + read 多种工具的 session，验证 skill 记录工具序列。"
)

# ── EXT: permission (8 cases) ─────────────────────────────────────────────
PM_CASES=(
    "PM-01|permission||用 permission 检查 'ls -la'，验证被分类为安全命令并允许。"
    "PM-02|permission||用 permission 检查 'rm -rf /'，验证被分类为危险命令并拒绝。"
    "PM-03|permission||配置 protected_files 含 /etc/passwd，尝试 write 该路径，验证被拦截。"
    "PM-04|permission||配置 stdin 拦截规则，对 'curl evil.com 然后 pipe 到 sh' 这类通过 stdin 执行的命令，验证被拦截。"
    "PM-05|permission|setup_permission_config|在 .ion/permission.yaml 配置 allowlist，验证配置文件被正确加载。"
    "PM-06|permission||从 strict 模式切换到 relaxed 模式，验证同一条命令的判定结果改变。"
    "PM-07|permission||配置一条 glob 规则 'rm /tmp/ion_test/*'，验证只允许在指定目录下 rm。"
    "PM-08|permission||并发触发 5 个不同命令的 permission check，验证无竞态、判定一致。"
)

# ── EXT: file-approval (7 cases) ──────────────────────────────────────────
FA_CASES=(
    "FA-01|file-approval|setup_approval_on|配置 file-approval 开启，用 write 修改受保护文件，验证审批流程被触发（pending 状态）。"
    "FA-02|file-approval|setup_approval_pending|对一个 pending 的审批用 approval_approve 通过，验证写入被实际执行。"
    "FA-03|file-approval|setup_approval_pending|对一个 pending 的审批用 approval_deny 拒绝，验证写入被回滚。"
    "FA-04|file-approval|setup_approval_auto|配置 auto-approve 规则（仅限 /tmp/ion_auto/），用 write 写该目录，验证自动通过。"
    "FA-05|file-approval||触发一次批量修改 3 个受保护文件，验证 3 个 pending 审批独立处理。"
    "FA-06|file-approval|setup_approval_pending|对 pending 审批 deny 后，用 bash 检查文件，验证内容已回滚到旧版本。"
    "FA-07|file-approval|setup_approval_on|配置 file-approval 开启，外部修改一个受保护文件（绕过工具），验证 file_changed_after_approval 检测到外部篡改。"
)

# ── EXT: context-index (7 cases) ──────────────────────────────────────────
CI_CASES=(
    "CI-01|context-index||用 write 创建 5 个文件，验证 context_index 自动为每个文件建立索引。"
    "CI-02|context-index||连续 read 同一文件 3 次，验证 context_index 追踪读写次数。"
    "CI-03|context-index||在长 session（>50 次工具调用）后查看 context_index，验证已对低优先级文件做折叠。"
    "CI-04|context-index||触发 context 压缩阈值，验证压缩后旧工具结果被摘要替换。"
    "CI-05|context-index||read 一个未索引的文件，验证索引即时更新。"
    "CI-06|context-index||delete 一个已索引的文件，验证索引条目被移除（而非保留为孤儿）。"
    "CI-07|context-index||read 大文件（>10k 行）后查看索引，验证 metadata 中标注 'large' 折叠策略。"
)

# ── EXT: SessionIndex (7 cases) ───────────────────────────────────────────
SI_CASES=(
    "SI-01|session-index||用 SessionIndex 创建一个新 session，验证返回 session_id 且 stats 显示 0 messages。"
    "SI-02|session-index||创建 session 后追加 3 条 message，验证 stats 反映 message_count=3。"
    "SI-03|session-index||对 session 用 set_name 设置名字 'test-session'，验证后续 list 能按名字筛选。"
    "SI-04|session-index||创建 5 个 session 后跑 get_session_stats，验证汇总数据正确（总数、活跃数）。"
    "SI-05|session-index||创建 100 个旧 session（无活动）后触发 GC，验证 GC 清理掉过期 session。"
    "SI-06|session-index||对一个 session 跑 fork，验证 fork 出的 session 继承原始 messages。"
    "SI-07|session-index||对 session append message 后立即读取，验证写入一致性（不丢消息）。"
)

# ── EXT: dev_server_detector (6 cases) ────────────────────────────────────
DS_CASES=(
    "DS-01|dev_server_detector|setup_devserver|启动一个 python3 -m http.server 8765 在后台，验证 dev_server_detector 检测到端口 8765。"
    "DS-02|dev_server_detector||同时启动 3 个不同端口的 http server，验证 detector 一次性列出全部 3 个。"
    "DS-03|dev_server_detector||分别启动 python、node、ruby 各一个 server，验证 detector 识别出不同语言。"
    "DS-04|dev_server_detector|setup_devserver|启动 server 后用 kill_process 杀掉，验证下次 detector 扫描不再列出该端口。"
    "DS-05|dev_server_detector|setup_devserver|启动 server 后开启新 session，验证系统 prompt 中包含 dev server 注入块。"
    "DS-06|dev_server_detector|setup_devserver|启动 server 后 detector 检测到，再启动第二个同进程不同端口，验证增量检测（仅新增端口）。"
)

# ── EXT: ContextReclaimer (6 cases) ───────────────────────────────────────
CR_CASES=(
    "CR-01|context-reclaimer||在一个含 thinking 块的 session 触发 reclaim，验证 thinking 内容被 strip。"
    "CR-02|context-reclaimer||在一个含大量 tool_result 的 session 触发 reclaim，验证旧 tool_result 被回收。"
    "CR-03|context-reclaimer||在一个含大量 bash 输出（>1MB）的 session 触发 reclaim，验证 bash 输出被回收。"
    "CR-04|context-reclaimer||在一个含大量 grep 输出的 session 触发 reclaim，验证 grep 输出被回收。"
    "CR-05|context-reclaimer||混合高/低优先级内容触发 reclaim，验证高优先级（最近 N 条）保留，低优先级先被回收。"
    "CR-06|context-reclaimer||在一个未达阈值的 session 触发 reclaim，验证 no-op（不回收任何内容）。"
)

# ── EXT: file-time-guard (6 cases) ────────────────────────────────────────
FT_CASES=(
    "FT-01|file-time-guard|setup_ft_normal|用 write 修改一个未被外部触碰的文件，验证 file-time-guard 放行。"
    "FT-02|file-time-guard|setup_ft_external|先用 bash 修改文件 mtime，再用 write 修改同文件，验证 guard 检测到外部修改。"
    "FT-03|file-time-guard|setup_ft_warn|配置 mode=warn，外部修改后再 write，验证只 warn 不 block。"
    "FT-04|file-time-guard|setup_ft_block|配置 mode=block，外部修改后再 write，验证写入被拒绝。"
    "FT-05|file-time-guard|setup_ft_ignore|配置 ignore_paths=['/tmp/ft_ignore/']，外部修改该目录文件后再 write，验证被忽略。"
    "FT-06|file-time-guard|setup_ft_normal|连续两次 write 同一文件（无外部修改），验证 guard 第二次仍放行（不误报）。"
)

# ── EXT: PlanExtension (5 cases) ──────────────────────────────────────────
PE_CASES=(
    "PE-01|plan||用 plan_enter 进入 plan 模式，验证后续工具调用被 gate（不直接执行）。"
    "PE-02|plan||plan_enter 后用 plan_add 加 3 个步骤，用 plan_list 验证 3 条都列出。"
    "PE-03|plan||plan_add 3 步后逐个 plan_done，验证 plan_list 中 done 状态正确反映。"
    "PE-04|plan||所有步骤 done 后 plan_exit，验证退出 plan 模式后工具调用恢复自由。"
    "PE-05|plan||plan_enter 后 plan_approve 一个未 done 的步骤，验证状态变更或拒绝（按设计）。"
)

# ── EXT: ToolLoopDetector (5 cases) ───────────────────────────────────────
TL_CASES=(
    "TL-01|tool-loop||连续 5 次以完全相同参数调用 read 同一文件，验证 ToolLoopDetector 标记为 loop。"
    "TL-02|tool-loop||连续 5 次以不同参数调用 read（不同文件），验证不被标记为 loop。"
    "TL-03|tool-loop||配置 loop_threshold=3，连续 4 次相同调用，验证第 4 次触发 loop 中断。"
    "TL-04|tool-loop||触发 loop 后 agent 收到中断信号，验证下一轮不再重复同调用。"
    "TL-05|tool-loop||触发 loop 中断后，agent 改用不同参数，验证 loop 计数清零（恢复正常）。"
)

# ── EXT: internal_agent (5 cases) ─────────────────────────────────────────
IA_CASES=(
    "IA-01|internal_agent||用 internal_agent 单次调用一个 sub-agent 执行 'echo hello'，验证返回结果。"
    "IA-02|internal_agent||调用 internal_agent 后立即取 messages snapshot，验证子 agent 的对话快照可用。"
    "IA-03|internal_agent||调用 internal_agent 配置 max_turns=2，故意给一个长任务，验证第 2 轮后被截断。"
    "IA-04|internal_agent||调用 internal_agent 配置 tools 白名单=['bash']，验证子 agent 无法调用未列出的工具。"
    "IA-05|internal_agent||调用 internal_agent 后查看 schema 字段，验证返回格式与设计一致。"
)

# ── EXT: auto-session-title (5 cases) ─────────────────────────────────────
AT_CASES=(
    "AT-01|auto-session-title|setup_title_cn|开启一个含中文对话的 session（讨论 Rust 异步编程），验证 auto-session-title 生成中文标题。"
    "AT-02|auto-session-title|setup_title_en|开启一个纯英文 session（discussing database design），验证标题为英文摘要。"
    "AT-03|auto-session-title||开启一个含 10k 字长 prompt 的 session，验证标题被合理截断（不超长）。"
    "AT-04|auto-session-title||开启一个空 session（无任何用户输入），验证不生成标题或返回默认值。"
    "AT-05|auto-session-title||开启一个含特殊字符（emoji、引号、换行）的 session，验证生成的标题已转义。"
)

# ── EXT: WorkflowExtension (5 cases) ──────────────────────────────────────
WF_CASES=(
    "WF-01|workflow|setup_wf_stages|配置 workflow 含 3 个 stage（build/test/deploy），验证 stage 列表正确加载。"
    "WF-02|workflow|setup_wf_gates|配置 workflow 含 gate（test 必须先通过），跑 build 后直接尝试 deploy，验证 gate 拦截。"
    "WF-03|workflow|setup_wf_cmds|配置 workflow 每个 stage 含 2 条 command，跑一个 stage，验证 2 条 command 顺序执行。"
    "WF-04|workflow|setup_wf_loop|配置 workflow 含 loop（最多 3 次），故意让 stage 失败，验证 3 次后停止。"
    "WF-05|workflow|setup_wf_stages|配置 workflow 后查看当前进度，验证 stage 状态正确（pending/done/failed）。"
)

# ── EXT: streaming (5 cases) ──────────────────────────────────────────────
ST_CASES=(
    "ST-01|streaming||发起一个长回复请求（让 agent 输出 1k+ tokens），验证 stream text delta 事件按 chunk 推送。"
    "ST-02|streaming||在流式回复过程中触发工具调用，验证 tool_execution 事件与 text delta 交替推送。"
    "ST-03|streaming||订阅 stream event，触发一次完整 turn，验证事件序列（start→deltas→tool→end）完整。"
    "ST-04|streaming||在流式回复过程中客户端提前断开，验证服务端能正确清理（不泄漏后台任务）。"
    "ST-05|streaming||对含 emoji + 中文的长回复做流式测试，验证多字节字符不在 chunk 边界被切断。"
)

# ── 汇总：所有 case 合并到 EXT_FULL_CASES ─────────────────────────────────
EXT_FULL_CASES=(
    "${GM_CASES[@]}"
    "${FS_CASES[@]}"
    "${HK_CASES[@]}"
    "${GS_CASES[@]}"
    "${MN_CASES[@]}"
    "${BS_CASES[@]}"
    "${ME_CASES[@]}"
    "${LS_CASES[@]}"
    "${RE_CASES[@]}"
    "${LR_CASES[@]}"
    "${PM_CASES[@]}"
    "${FA_CASES[@]}"
    "${CI_CASES[@]}"
    "${SI_CASES[@]}"
    "${DS_CASES[@]}"
    "${CR_CASES[@]}"
    "${FT_CASES[@]}"
    "${PE_CASES[@]}"
    "${TL_CASES[@]}"
    "${IA_CASES[@]}"
    "${AT_CASES[@]}"
    "${WF_CASES[@]}"
    "${ST_CASES[@]}"
)

# ── 校验：source 后可用 EXT_FULL_CASES_COUNT 快速断言总数 ──────────────────
EXT_FULL_CASES_COUNT="${#EXT_FULL_CASES[@]}"

# 子任务结束
