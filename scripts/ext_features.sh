#!/usr/bin/env bash
# ext_features.sh - Multi-turn dialog feature points

FP_02A=(
    "FP-02A|EXT-02|save + search 基本流|02-M1,02-M2,02-M3,02-M4"
    "帮我记住一条信息：用户喜欢 Rust 语言，用 memory_save 保存 tags 设为 language,rust"
    "搜一下记忆里有没有 rust 相关的内容"
)

FP_02B=(
    "FP-02B|EXT-02|跨项目检索 + 中文 fallback|02-M3,02-M4,02-M6"
    "保存一条记忆：项目技术栈用 SQLite 和 tokio，category 设为 stack"
    "用 global=true 搜一下 rust，看看跨项目的记忆有没有"
    "再搜一个中文词「技术栈」，看能不能命中"
)

FP_02C=(
    "FP-02C|EXT-02|list + forget 生命周期|02-M1,02-M3,02-M7"
    "先用 memory_save 工具保存一条记忆：内容是「临时测试条目，可删」，tags 设为 temp,test（这条等下用来演示删除）"
    "用 memory_search 搜一下 temp，把刚才保存的那条列出来"
    "把刚才那条 temp 记忆忘掉，用 memory_forget（或 memory_delete）删掉它，再搜一次 temp 确认已经删没了"
)

FP_02D=(
    "FP-02D|EXT-02|错误处理 + 空查询|02-M3,02-M8"
    "搜一个肯定不存在的关键字 zzzqqqxx，看看返回什么"
    "保存一条空内容的记忆，看看会不会报错"
)

FP_03A=(
    "FP-03A|EXT-03|stdout 正则检测端口|03-M1,03-M2,03-M3"
    "用 bash background=true 启动 python3 -m http.server 8765"
    "现在你知道有哪些 dev server 在跑吗？说一下"
)

FP_03B=(
    "FP-03B|EXT-03|多端口并发 + 探测|03-M1,03-M2,03-M5"
    "后台再起两个 http server，分别用 8766 和 8767 端口"
    "用 extension_rpc 查一下当前检测到的所有 dev server"
    "把你看到的 dev server 端口都报给我"
)

FP_03C=(
    "FP-03C|EXT-03|清理 + 进程退出|03-M4,03-M6"
    "把刚才起的那几个后台 server 都停掉"
    "清一下 dev_server_detector 的检测记录"
    "再查一次，应该没有残留的 server 了"
)

FP_04A=(
    "FP-04A|EXT-04|单文件创建快照|04-M1,04-M2,04-M3,04-M4"
    "在当前目录创建一个文件 hello.txt，内容写 hello world"
    "再创建一个 test.rs，写一行注释 // snapshot test"
    "你刚才创建文件的时候，系统有没有自动记录快照？说一下"
)

FP_04B=(
    "FP-04B|EXT-04|编辑覆盖 + diff|04-M1,04-M3,04-M5"
    "用 write 工具覆盖 hello.txt，内容改成 hello snapshot updated"
    "这次修改系统应该记了 diff，描述一下前后差异"
)

FP_04C=(
    "FP-04C|EXT-04|bash 兜底 + 删除捕获|04-M2,04-M3,04-M7"
    "用 bash 命令直接删掉 test.rs 这个文件"
    "用 bash echo 追加一行到 hello.txt（绕过 write 工具）"
    "系统应该也能捕获到 bash 改的文件，确认一下"
)

FP_05A=(
    "FP-05A|EXT-05|干净代码诊断|05-M1,05-M2,05-M6"
    "用 write 工具在 src/lib.rs 写一个正确的 add 函数，接受两个 i32 返回和"
    "用 lsp_check 检查一下，应该没有错误"
)

FP_05B=(
    "FP-05B|EXT-05|编译错误捕获 + 修复闭环|05-M1,05-M3,05-M4"
    "改一下 src/lib.rs，加一行 let x: i32 = \"string\"; 故意制造一个类型错误"
    "用 lsp_check 看看能不能报出来这个错误"
    "把这个错误修掉，再 lsp_check 确认错误清零"
)

FP_05C=(
    "FP-05C|EXT-05|警告分类 + 自动注入|05-M1,05-M5,05-M6"
    "在 src/lib.rs 里加一个没用过的变量 let unused = 42;"
    "lsp_check 一下，应该是 warning 不是 error"
)

FP_06A=(
    "FP-06A|EXT-06|PostToolUse command 触发|06-M1,06-M2,06-M8"
    "我看你配了 PostToolUse hook，随便写个文件触发一下，看看 hook 日志有没有记录"
    "再读一下那个 hook 输出日志文件，确认 hook 真的执行了"
)

FP_06B=(
    "FP-06B|EXT-06|PreToolUse matcher 过滤|06-M2,06-M3"
    "你的 hook 配了只匹配 bash 工具，现在跑一个 bash 命令 echo hello"
    "再写一个文件（write 工具），这个不应该触发 bash-only 的 hook，确认一下"
)

FP_06C=(
    "FP-06C|EXT-06|Stop 事件 + block 注入|06-M1,06-M6,06-M8"
    "你配了 Stop hook 会 block，试着结束任务看看会不会被拦回来"
    "如果被拦回来了，说一下 hook 给的理由是什么"
)

FP_07A=(
    "FP-07A|EXT-07|goal_set + 自动 check 生成|07-M1,07-M2,07-M3"
    "用 goal_set 设一个目标：在当前目录创建一个 greet.rs 文件，里面写一个 greet 函数打印 hello"
    "目标设好了之后，系统应该自动生成了验证 check，列一下有哪些 check"
)

FP_07B=(
    "FP-07B|EXT-07|闭环迭代 + gate 拦截|07-M1,07-M4,07-M5"
    "现在开始实现这个目标，创建 greet.rs 并写 greet 函数"
    "做完想停的时候，系统会跑 check 验证，如果没过会拦住你，继续修直到通过"
)

FP_07C=(
    "FP-07C|EXT-07|goal_set + refine 动态调整|07-M1"
    "先用 goal_set 工具设定一个目标：在当前目录创建 greet.rs，里面写一个 greet 函数打印 hello。系统会自动生成验证 check"
    "用 goal_refine 工具把目标改成：greet 函数要接受一个 name 参数，打印 hello name。确认 check 列表也跟着更新了"
    "继续做，用 write 工具按新目标创建 greet.rs 实现 greet(name)，让新 check 通过"
)

FP_08A=(
    "FP-08A|EXT-08|list + status 查看|08-M1,08-M2"
    "用 extension_rpc 查一下当前有哪些 monitor 定义"
    "再看一下各 monitor 的运行状态（trigger_count / last_result）"
)

FP_08B=(
    "FP-08B|EXT-08|add + validate + test dry-run|08-M1,08-M3,08-M4"
    "加一个新 monitor 名叫 test-disk，脚本用 df -h | grep -v Filesystem | head -1，间隔 60 秒，触发模式 event_only"
    "用 validate 验证一下这个 monitor 定义合不合法"
    "用 test 干跑一下这个脚本，看 would_trigger 是不是 true"
)

FP_08C=(
    "FP-08C|EXT-08|active pipeline 生命周期|08-M1,08-M5"
    "用 mark_active 标记 monitor=test-disk key=test-1 正在处理"
    "用 check_active 确认它被标记成 active 了"
    "用 release_active 释放掉，再用 list_active 看还有没有"
)

FP_08D=(
    "FP-08D|EXT-08|remove + 错误处理|08-M1"
    "把 test-disk 这个 monitor 删掉"
    "再删一次同名 monitor，应该报 not found 的错"
)

FP_09A=(
    "FP-09A|EXT-09|后台 bash + 进程管理|09-M1,09-M2"
    "用 bash 工具（参数 background=true）后台启动一个 python3 -m http.server 8888，确认返回了一个 bid 进程号"
    "用 get_background_process 列出所有后台进程，把刚才那个 bid 的状态告诉我"
    "用 kill_process 把那个 bid 停掉，确认进程已经退出"
)

FP_09B=(
    "FP-09B|EXT-09|后台进程 + 管理|09-M1,09-M3,09-M4"
    "后台启动一个 sleep 300 命令"
    "用 get_background_process 列一下后台进程，拿到 bid"
    "用 inspect 看那个 bid 的详情，再用 kill_process 停掉它"
)

FP_09C=(
    "FP-09C|EXT-09|write_stdin 交互|09-M1,09-M5"
    "后台跑一个 cat 命令（会等 stdin 输入）"
    "用 write_stdin 给它发一句 hello from stdin"
    "inspect 看看输出里有没有那句话，然后 kill 掉"
)

FP_09D=(
    "FP-09D|EXT-09|timeoutBackground + stderr 合并|09-M1"
    "跑一个会超时的命令 sleep 100，用 timeoutBackground=true 让它超时后自动转后台"
    "跑一个 python3 -c 'print(\"out\"); import sys; sys.stderr.write(\"err\\n\")'，确认 stderr 也被合并到输出里了"
)

FP_10A=(
    "FP-10A|EXT-10|记忆保存与检索|"
    "请记住一条偏好：这个项目所有新增的 Rust 函数都要写 doc comment，类别标 rust-convention，标签 rust、doc"
    "我刚才让你记的那条关于 doc comment 的偏好，能帮我查出来吗？"
    "搜一下 'doc comment'，看能不能匹配到"
)

FP_10B=(
    "FP-10B|EXT-10|被动上下文注入|"
    "帮我记一下：数据库连接串格式是 postgres://user:pass@host:5432/db，标签 database"
    "我想连数据库，连接串格式是什么来着？"
    "再问个无关的：今天天气怎么样？"
)

FP_10C=(
    "FP-10C|EXT-10|RPC 管理接口|"
    "用 extension_rpc memory list 看看当前有哪些记忆条目"
    "把我刚才记的那条数据库连接串记忆 forget 掉"
    "再 list 一次确认它已经被归档了"
)

FP_10D=(
    "FP-10D|EXT-10|跨项目全局搜索|"
    "搜一下 'ci pipeline 配置'，只在本项目里搜"
    "同样这个关键词，改成跨项目全局搜（global=true）"
    "对比两次结果，全局搜应该多出其他项目的命中"
)

FP_11A=(
    "FP-11A|EXT-11|全局规则注入 system prompt|"
    "我在 .ion/rules/global.md 写了一条 globs: '**' 的规则：'回答前先复述用户问题'。新建会话后问我 1+1 等于几"
    "确认第一轮回答里能看到这条规则被遵循了"
)

FP_11B=(
    "FP-11B|EXT-11|路径匹配规则按需注入|"
    "我在 .ion/rules/rust.md 写了 globs: '**/*.rs'，内容是 'Rust 文件必须用 snake_case'。然后帮我读一下 src/lib.rs"
    "读完后，tool result 末尾应该追加这条 rust 规则（📌 [project rules for this file]）"
    "再读一个 .md 文件，确认不会追加 rust 规则"
)

FP_11C=(
    "FP-11C|EXT-11|RPC 查询规则|"
    "用 extension_rpc 调 rules-engine 的 list 方法，列出所有规则"
    "用 match 方法查 'src/main.rs' 命中哪些规则"
    "用 match 方法查 'README.md'，应该不命中 rust 规则"
)

FP_11D=(
    "FP-11D|EXT-11|规则热重载|"
    "先 list 一下当前规则"
    "现在我去 .ion/rules/ 新建一个 hot.md（globs: '**'，内容 '热重载测试'），再问你一句话"
    "再次 list，应该能看到新增的 hot 规则；system prompt 里也应该注入了"
)

FP_12A=(
    "FP-12A|EXT-12|实质工作会话触发提炼|"
    "帮我读 src/lib.rs 看看结构，然后把里面的 main 函数名改成 run_main"
    "再帮我加一个测试用例验证 run_main 能被调用"
    "好的结束吧，我去查 skill distillation 的日志和产物目录"
)

FP_12B=(
    "FP-12B|EXT-12|短会话/问候跳过提炼|"
    "hi"
    "ok thanks"
    # 退出后日志应显示 skip: too few messages 或 all messages are greetings
)

FP_12C=(
    "FP-12C|EXT-12|secret 自动脱敏|"
    "我这里有个 key sk-proj-abcdef1234567890ABCDEFGHIJ，帮我存到 .env 文件里"
    "再读一下这个 .env 确认存好了"
    "退出后查日志，应该看到 [learning] redacted N secret(s) from message"
)

FP_13A=(
    "FP-13A|EXT-13|添加规则并自动生效|"
    "用 extension_rpc permission add_rule 加一条：subject=file.read, pattern=*.env, decision=deny, scope=session"
    "然后帮我读一下 .env 文件"
    "再读一个普通文件 like Cargo.toml，应该正常"
)

FP_13B=(
    "FP-13B|EXT-13|stored-decision 持久化|"
    "用 store_decision 存一条：command.run, git status, allow, project"
    "list_stored 看一下，应该有 source=stored 的条目"
    "退出后去项目 .ion/settings.json 确认持久化了"
)

FP_13C=(
    "FP-13C|EXT-13|撤销 stored 决策|"
    "先 store_decision 两条 allow 规则（不同 pattern）"
    "list_stored 拿到第一条的 id，用 remove_stored 删掉它"
    "再 clear_stored 清剩下的，最后 list_stored 应为空、list_rules 里 Config 规则还在"
)

FP_13D=(
    "FP-13D|EXT-13|settings.json 热重载|"
    "先 list_rules 看当前规则"
    "我手动在 .ion/settings.json 的 permissions.rules 里加一条 allow bash 'echo hi' 的规则。现在帮我跑 echo hi"
    "应该不再弹权限确认（自动 allow）；再 list_rules 应该多出这条"
)

FP_14A=(
    "FP-14A|EXT-14|变更列出与 pending 计算|"
    "用 write 工具在项目根创建文件 scratch.rs，内容写一行 fn main() { println!(\"hello\"); }"
    "再用 write 工具改一下 src/lib.rs，在文件末尾追加一行注释 // touched"
    "停下，用 extension_rpc 查 file-approval 的 pending 列表，应该有两个文件（added + modified）"
)

FP_14B=(
    "FP-14B|EXT-14|approve 锚定 baseline|"
    "先用 write 工具创建文件 /tmp/fa_b_demo.txt，内容写 baseline content"
    "用 extension_rpc 调 file-approval approve，参数 path=/tmp/fa_b_demo.txt，把这条变更批准掉"
    "再用 write 工具覆盖 /tmp/fa_b_demo.txt，改成 second content，然后查 pending，diff 应该只显示这次新写入的内容（baseline 已锚定到上次 approve）"
)

FP_14C=(
    "FP-14C|EXT-14|reject 单文件回滚|"
    "用 write 工具新建文件 /tmp/fa_c_reject.txt，内容写点 will be rejected"
    "用 extension_rpc 查 file-approval pending 确认有这个 added 文件"
    "用 extension_rpc 调 file-approval reject，参数 path=/tmp/fa_c_reject.txt，文件应该从磁盘消失了"
)

FP_14D=(
    "FP-14D|EXT-14|re-approval 重置|"
    "先用 write 工具创建文件 /tmp/fa_d_reset.txt 内容写 first version"
    "用 extension_rpc 调 file-approval approve，参数 path=/tmp/fa_d_reset.txt 把它批准掉"
    "再用 write 工具覆盖 /tmp/fa_d_reset.txt 改成 second version；用 extension_rpc 查 pending，这个文件应该重新出现（status=pending），并能看到 ApprovalReset 事件"
)

FP_15A=(
    "FP-15A|EXT-15|read 建立索引并注入 tree|"
    "帮我读 src/lib.rs 和 src/main.rs 两个文件"
    "用 extension_rpc context-index tree 查一下，这两个文件应该都在索引里，status=current"
    "system prompt 里应该能看到 <context_index> 块列出这两个文件"
)

FP_15B=(
    "FP-15B|EXT-15|write 后旧 read 折叠|"
    "先读 src/lib.rs"
    "现在 edit 一下 src/lib.rs 改点东西"
    "用 context-index ranges 查 src/lib.rs，应该有 stale 记录；之后再读它时旧的 ToolResult 已被折叠成占位符"
)

FP_15C=(
    "FP-15C|EXT-15|grep 输出进索引|"
    "用 grep 搜一下 'fn main' 这个关键词"
    "用 context-index tree 查，被 grep 命中的文件应该都在索引里"
    "确认 untracked 列表里没有 grep（只有 bash/find）"
)

FP_15D=(
    "FP-15D|EXT-15|RPC ranges 查询|"
    "读同一个文件两次（不同 turn）"
    "edit 一次该文件"
    "用 ranges 查这个 path，应该看到两条 read（一条 current 一条 stale）+ stale 的 overwrittenByTurn"
)

FP_16A=(
    "FP-16A|EXT-16|会话列表与项目过滤|"
    "跑 ion sessions，看看当前项目的会话列表"
    "跑 ion sessions --all，应该能看到其他项目的会话"
    "跑 ion sessions --json --limit 5，确认 JSON 字段完整（token/turn_count/created_at 等）"
)

FP_16B=(
    "FP-16B|EXT-16|会话命名与重命名|"
    "新建会话时用 --name feature-x 启动"
    "跑 ion sessions 确认名字显示为 feature-x"
    "用 extension_rpc 把它重命名为 feature-x-v2，再 list 确认 name 变了、first_name 还是 feature-x"
)

FP_16C=(
    "FP-16C|EXT-16|统计增量与累计|"
    "新建一个会话，先问一句简单的话记录基线"
    "再连续问 2-3 个会触发工具调用的问题"
    "用 ion sessions --json 查这个会话，turn_count/token_input 应该累加正确，大于第一轮"
)

FP_16D=(
    "FP-16D|EXT-16|父子血缘|"
    "在当前会话里 fork 一个子任务（spawn_worker 或类似）"
    "用 ion sessions --json 查子会话，parent_session 应指向当前会话 id"
    "跑 ion session tree <当前会话id>，应该能看到子会话节点"
)

FP_17A=(
    "FP-17A|EXT-17|大量 bash 输出触发回收|"
    "连续跑 10 次 cat 一个大文件（每次输出几百行）"
    "再跑几次 ls -la 列大量文件"
    "现在回看最早几轮的 bash 结果，应该已经被折叠成 [reclaimed: ...] 占位符；查 tracing 日志应该有 [reclaimer] X → Y tokens (saved Z)"
)

FP_17B=(
    "FP-17B|EXT-17|thinking block 自动剥离|"
    "用一个会触发 thinking 的中难度问题（比如让它推理一个算法），开 medium thinking"
    "再连续问几个问题让上下文变长"
    "回看会话历史，Assistant 消息里不应该再保留 thinking 块（被剥离）；tracing 日志 thinking_blocks_removed > 0"
)

FP_17C=(
    "FP-17C|EXT-17|stale read 即使在 heat window 内也回收|"
    "读一个大文件（比如 src/lib.rs）"
    "紧接着 edit 这个文件"
    "继续问几轮别的问题让上下文滚动，然后查最早那个 read 的 ToolResult —— 即使在 heat window 内也应该被标 stale 并折叠（reason 里是 stale 不是 old）"
)

FP_17D=(
    "FP-17D|EXT-17|heat window 保护最近消息|"
    "制造一段很长的会话历史（混合 bash/grep/read）"
    "持续问问题直到触发回收"
    "查最近 6 条消息，全部应保持原样不被折叠；越旧的非 stale 消息按 bash>grep>read 顺序被回收"
)

FP_18A=(
    "FP-18A|EXT-18|Warn 模式默认：read 后外部修改触发 stale 警告|"
    "帮我读一下 /tmp/ftg_demo.txt 这个文件，告诉我里面写了什么内容。"
    "现在帮我在 /tmp/ftg_demo.txt 末尾追加一行 'agent updated'。"
)

FP_18B=(
    "FP-18B|EXT-18|ignore_paths：target/ 路径下文件变更不报警|"
    "读一下 /tmp/ftg_target_demo/target/x.rs，确认文件存在。"
    "帮我把 /tmp/ftg_target_demo/target/x.rs 内容改成 '// regenerated'。"
)

FP_18C=(
    "FP-18C|EXT-18|RPC status/check：查询 guard 模式与 staleness|"
    "用 extension_rpc 调用 'file-time-guard' 扩展的 status 方法，把返回的 mode 和 tracked_files 报给我。"
    "再用 extension_rpc 调用 check 方法，参数 path='/tmp/ftg_rpc_demo.txt'，告诉我 stale 是 true 还是 false。"
)

FP_19A=(
    "FP-19A|EXT-19|完整 plan 流程：enter→add×3→list→approve→done→exit→落盘|"
    "进入 plan mode，plan_path 用 /tmp/ion_plan_a.md，然后依次 plan_add 三个步骤：'分析需求'、'写代码'、'跑测试'。加完后 plan_list 给我看一眼。"
    "把第 0 步 plan_approve 一下，再 plan_done 标记完成，最后 plan_exit 退出 plan mode。退出后读 /tmp/ion_plan_a.md 确认内容写进去了。"
)

FP_19B=(
    "FP-19B|EXT-19|plan mode 下 write/edit/bash 被拦截|"
    "先用 plan_enter 进入 plan mode（plan_path=/tmp/ion_plan_b.md）。然后尝试直接 write 一个 /tmp/blocked_in_plan.txt 文件，把结果告诉我。"
    "现在 plan_exit 退出 plan mode，退出后重新 write /tmp/blocked_in_plan.txt 写一句 'now allowed'，应该就能成功了。"
)

FP_19C=(
    "FP-19C|EXT-19|strict_mode 闸门：未 approve 不能 exit|"
    "plan_enter 进入 plan mode，带上参数 strict_mode=true，plan_path=/tmp/ion_plan_c.md。然后 plan_add 两个步骤：'step one'、'step two'，先不要 approve。"
    "现在尝试 plan_exit，应该会被 strict_mode 拦下。接着 plan_approve 第 0 步和第 1 步，再 plan_exit，这次应该能成功退出。"
)

FP_20A=(
    "FP-20A|EXT-20|连续 read 同一文件触发 ABORT|"
    "请连续 6 次读取 /tmp/loop_target.txt 这个文件，每次读完都说一句 'read again'，直到系统阻止你。"
    "刚才是不是被 loop detector 拦下了？用 grep 搜一下 'loop' 关键字，确认一下行为日志里有没有 WARN/ABORT 记录。"
)

FP_20B=(
    "FP-20B|EXT-20|豁免工具 plan_list 连续调用不触发|"
    "先 plan_enter 进入 plan mode（plan_path=/tmp/ion_loop_exempt.md），加一个步骤 'demo step'。然后连续调用 plan_list 6 次给我看，每次都把结果报一遍。"
    "好的，现在 plan_exit 退出。整轮下来你应该没收到任何 'Tool loop detected' 的错误，对吧？"
)

FP_20C=(
    "FP-20C|EXT-20|连续相同 bash echo 归一化触发 ABORT（验证 normalize）|"
    "连续执行 6 次 bash 命令，每次 echo 一句不同的话（比如 echo one、echo two、echo three …），每次都告诉我返回了什么，直到被拦下。"
    "被拦之后，改成连续 6 次 ls /tmp（同一条命令），观察是不是也会被拦。"
)

FP_22A=(
    "FP-22A|EXT-22|LLM 生成会话标题|"
    "用 Rust 写一个最简单的 TCP echo server，监听 127.0.0.1:7878，把收到的字节原样回写。"
    "再补一段注释说明为什么用 std::net::TcpListener 而不是 tokio。"
)

FP_22B=(
    "FP-22B|EXT-22|启发式 fallback：中文冒号截断|"
    "按以下 10 步顺序执行：1. 用 bash 创建文件 2. 写入内容 3. 读回来核对"
    "等一下，先别执行，我想先看看你打算怎么做。"
)

FP_22C=(
    "FP-22C|EXT-22|启发式 fallback：命令前缀原样保留|"
    "!ls -la /tmp 看看里面都有什么文件"
    "!rm -rf /tmp/acceptance_scratch 2>/dev/null; 现在再看一遍 !ls /tmp"
)

FP_23A=(
    "FP-23A|EXT-23|gate 失败触发 RetryWith 强制继续|"
    "（前置：--agent /tmp/wf_fail.md，其 workflow.gate_command='ls /tmp/wf_done_marker'，gate_expected='wf_done_marker'）请完成任务：确保 /tmp/wf_done_marker 这个标记文件被创建。"
    "现在重新检查一次 gate，确认它已经 PASS。"
)

FP_23B=(
    "FP-23B|EXT-23|max_retries 耗尽后放行（防死循环）|"
    "（前置：--agent /tmp/wf_exhaust.md，gate_command='echo NOPE'，gate_expected='YES'，max_retries=2）随便回答一句 'done' 就行，不用真的做什么。"
    "再发一句 'really done'，确认会话能正常结束。"
)

FP_23C=(
    "FP-23C|EXT-23|gate 通过后缓存：后续检查直接 Allow|"
    "（前置：--agent /tmp/wf_pass.md，gate_command='echo PASS'，gate_expected='PASS'）回复一句 'hello' 就好。"
    "再说一句 'world'。"
)

FP_24A=(
    "FP-24A|EXT-24|subscribe --session 按 sessionId 过滤|"
    "（评测并行起一个 'ion subscribe --session <SID>' 后台进程）读一下 README.md 的前 50 行，告诉我这个项目大概在做什么。"
    "再用 grep 搜一下 'StreamingExtension' 在 src/worker_rpc.rs 里的位置。"
)

FP_24B=(
    "FP-24B|EXT-24|事件类型完整性：start/delta/end 全链路|"
    "用 bash 执行 'echo streaming_check'，把输出报给我。"
    "再读一下 Cargo.toml 的 [package] 段。"
)

FP_24C=(
    "FP-24C|EXT-24|tool_call_delta 增量透传|"
    "（前置：环境变量 ION_STREAM_DEBUG=1）写一个文件 /tmp/stream_delta_demo.txt，内容是一段 200 字以上的长文本（你随便编），用 write 工具。"
    "再把刚才写的文件 read 回来确认。"
)


# ── get_all_features: output all feature points, one per line ──
# Format: EXT-XX|FP-XXA|feature_name|expected_metrics|turns_count|turn1~turn2~...
get_all_features() {
    local arr_name
    for arr_name in $(compgen -v 2>/dev/null | grep '^FP_'); do
        local header="" turns_str=""
        eval "local vals=(\"\${$arr_name[@]}\")"
        [ ${#vals[@]} -lt 2 ] && continue
        header="${vals[0]}"
        local tarr=()
        local i
        for ((i=1; i<${#vals[@]}; i++)); do
            tarr+=("${vals[$i]}")
        done
        turns_str=$(IFS='~'; echo "${tarr[*]}")
        local nturns=${#tarr[@]}
        echo "${header}|${nturns}|${turns_str}"
    done
}

get_features_for_ext() {
    local ext_id="$1"
    get_all_features | grep "^${ext_id}|"
}
