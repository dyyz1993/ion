# Context Files（AGENTS.md 加载）CLI/CI 测试规格

> **版本**：v0.2（P0 + 边界补充）
> **功能**：`ContextFilesExtension` —— 向上递归加载 `AGENTS.md` / `CLAUDE.md` / `GEMINI.md`，通过 `on_system_prompt` 注入项目规范。
> **前置依赖**：无（P0 是所有 Workspace 方案的前置）
> **对齐模板**：[CLI_TEST_TEMPLATE.md](../templates/CLI_TEST_TEMPLATE.md) + 参考 [RULES_CI_SPEC.md](./RULES_CI_SPEC.md) 格式

---

## 一、测试覆盖矩阵

### Group A：基础加载（文件扫描 + 注入）

| ID | 验证点 | CLI 命令 | 预期 |
|----|--------|---------|------|
| A1 | git 根有 AGENTS.md → 注入 system prompt | `ion --no-session "x"` → `ion --session <sid> --export out.html` | systemPrompt 含 `--- project context ---`，contextFiles 非空 |
| A2 | 无任何 context 文件 → 不报错 | 在空目录跑 `ion --no-session "x"` | 退出码 0，无 panic，systemPrompt 无 project context |
| A3 | session.jsonl 无 system_prompt entry → export 重建 | `ion --session <sid> --export` | export 注入 AGENTS.md 到 systemPrompt |

### Group B：文件查找逻辑（向上递归 + 优先级 + 边界）

| ID | 验证点 | 目录结构 | 预期 |
|----|--------|---------|------|
| B1 | 从子目录向上找到 git 根的 AGENTS.md | `<root>/src/agent/` ← 跑 ion，root 有 .git + AGENTS.md | systemPrompt 含 git 根 AGENTS.md 内容 |
| B2 | AGENTS.md 优先于 CLAUDE.md | 同目录放 AGENTS.md + CLAUDE.md | 只加载 AGENTS.md |
| B3 | 无 AGENTS.md → 降级 CLAUDE.md | 只有 CLAUDE.md | 加载 CLAUDE.md |
| B4 | 多级目录：根在前，叶子在后 | root + packages/core/ 各放 AGENTS.md | 排序为 [root, packages/core] |
| B5 | 遇到 .git 停止向上递归 | workspace/AGENTS.md + workspace/project/.git + workspace/project/AGENTS.md | 只加载 project/ 的，不加载 workspace/ 的 |
| B6 | GEMINI.md 降级（最低优先级） | 只有 GEMINI.md | 加载 GEMINI.md |
| B7 | 3 文件同目录 → AGENTS 胜出 | AGENTS.md + CLAUDE.md + GEMINI.md 同目录 | 只加载 AGENTS.md |
| B8 | 三级目录嵌套全加载 | root/a/b/c/ 各层有 AGENTS.md，从 c/ 跑 | 加载 3 个，排序 root→mid→leaf |
| B9 | .git 是目录而非文件（真实 git init） | `git init` 创建 .git 目录 | 能识别为 git 根，正常加载 |
| B10 | AGENTS.md 为空文件 | AGENTS.md 内容为空 | 空文件也加载，段落头正常 |
| B11 | AGENTS.md 只有 1 行 | AGENTS.md = "just one line" | 正常加载单行内容 |

### Group C：开关 + 边界 + 预算

| ID | 验证点 | CLI 命令 | 预期 |
|----|--------|---------|------|
| C1 | `--no-context-files` 关闭注入 | `ion --no-context-files --session <sid> --export` | systemPrompt 无 project context，contextFiles 为空 |
| C2 | `ION_NO_CONTEXT_FILES=1` env 关闭 | `ION_NO_CONTEXT_FILES=1 ion --session <sid> --export` | 同 C1 |
| C3 | 超大文件截断（UTF-8 安全） | `ION_CONTEXT_FILES_MAX_CHARS=500 ion --no-session "x"` | systemPrompt 含 `truncated` 标记 |
| C4 | 默认预算 12000 字符不截断 | `ion --no-session "x"` → export | AGENTS.md 完整加载（无 truncated） |
| C5 | CLAUDE.md 优先于 GEMINI.md（无 AGENTS） | 同目录 CLAUDE.md + GEMINI.md | 只加载 CLAUDE.md |
| C6 | 大预算（50000）不截断 | `ION_CONTEXT_FILES_MAX_CHARS=50000` | 5000 字符内容完整加载 |
| C7 | `--no-context-files` 运行时 sidecar 不含 context | `ion --no-context-files --no-session "x"` → sidecar | sidecar 无 project context 段落 |

### Group D：导出 HTML 面板呈现

| ID | 验证点 | CLI 命令 | 预期 |
|----|--------|---------|------|
| D1 | 面板显示 Context Files 字段（ON） | `ion --session <sid> --export out.html` → 浏览器查看 | 面板显示 `Context Files: AGENTS.md` |
| D2 | 面板显示 none（OFF） | `ion --no-context-files --session <sid> --export` | 面板显示 `Context Files: none` |
| D3 | system prompt 折叠面板含 AGENTS.md 全文 | export HTML → 展开 System Prompt 面板 | 含 `--- project context ---` 段落 + AGENTS.md 内容 |

### Group E：单元测试（回归保障）

| ID | 验证点 | 命令 | 预期 |
|----|--------|------|------|
| E1 | P0 扩展 23 个单元测试 | `cargo test --lib context_files_extension` | 23 passed |
| E2 | rules_engine 回归 | `cargo test --lib rules_engine` | 28 passed |
| E3 | export 回归 | `cargo test --lib export` | 28 passed |

---

## 二、Group A：基础加载

### A1 git 根有 AGENTS.md → 注入 system prompt

```bash
# 1. 确认项目根有 AGENTS.md
ls AGENTS.md

# 2. 跑会话（FauxProvider，不需要真实 API key）
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "smoke test"

# 3. 找到 session
SID=$(basename $(ls -t ~/.ion/agent/sessions/*ion*/*.jsonl | head -1) .jsonl)

# 4. 导出
ION_API_KEY=fake ion --session "$SID" --export /tmp/a1.html

# 5. 验证
python3 -c "
import base64, re, json
with open('/tmp/a1.html') as f: h = f.read()
idx = h.find('eyJ')
data = json.loads(base64.b64decode(re.match(r'[A-Za-z0-9+/=]+', h[idx:idx+300000]).group(0).encode()).decode())
print('含 project context:', '--- project context ---' in data['systemPrompt'])
print('contextFiles:', data.get('contextFiles', []))
"
```

**预期输出：**
```
含 project context: True
contextFiles: ['/Users/.../AGENTS.md']
```

---

### A2 无任何 context 文件 → 不报错

```bash
TMPDIR=$(mktemp -d /tmp/ion_a2_XXXX)
cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "x"
echo "退出码: $?"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
ION_API_KEY=fake ion --session "$SID" --export /tmp/a2.html
python3 -c "
import base64, re, json
with open('/tmp/a2.html') as f: h = f.read()
idx = h.find('eyJ')
data = json.loads(base64.b64decode(re.match(r'[A-Za-z0-9+/=]+', h[idx:idx+300000]).group(0).encode()).decode())
print('含 project context:', '--- project context ---' in data.get('systemPrompt',''))
"
cd - && rm -rf "$TMPDIR"
```

**预期输出：**
```
退出码: 0
含 project context: False
```

---

## 三、Group B：文件查找逻辑

### B1 从子目录向上找到 git 根的 AGENTS.md

```bash
TMPDIR=$(mktemp -d /tmp/ion_b1_XXXX)
mkdir -p "$TMPDIR/src/agent"
echo "" > "$TMPDIR/.git"
echo "# Root AGENTS" > "$TMPDIR/AGENTS.md"

cd "$TMPDIR/src/agent"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "subdir test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
grep -c "Root AGENTS" "$SIDECAR"  # 预期: 1

cd - && rm -rf "$TMPDIR"
```

---

### B2 AGENTS.md 优先于 CLAUDE.md

```bash
TMPDIR=$(mktemp -d /tmp/ion_b2_XXXX)
echo "" > "$TMPDIR/.git"
echo "# AGENTS content" > "$TMPDIR/AGENTS.md"
echo "# CLAUDE content" > "$TMPDIR/CLAUDE.md"

cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "priority test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
echo "AGENTS: $(grep -c 'AGENTS content' "$SIDECAR")"  # 预期: 1
echo "CLAUDE: $(grep -c 'CLAUDE content' "$SIDECAR")"  # 预期: 0

cd - && rm -rf "$TMPDIR"
```

---

### B3 无 AGENTS.md → 降级 CLAUDE.md

```bash
TMPDIR=$(mktemp -d /tmp/ion_b3_XXXX)
echo "" > "$TMPDIR/.git"
echo "# CLAUDE only" > "$TMPDIR/CLAUDE.md"

cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "fallback test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
grep -c "CLAUDE only" "$SIDECAR"  # 预期: 1

cd - && rm -rf "$TMPDIR"
```

---

### B4 多级目录：根在前，叶子在后

```bash
TMPDIR=$(mktemp -d /tmp/ion_b4_XXXX)
echo "" > "$TMPDIR/.git"
echo "# ROOT" > "$TMPDIR/AGENTS.md"
mkdir -p "$TMPDIR/packages/core"
echo "# LEAF" > "$TMPDIR/packages/core/AGENTS.md"

cd "$TMPDIR/packages/core"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "multi-level test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
python3 -c "
with open('$SIDECAR') as f: sp = f.read()
print('ROOT 在前:', sp.find('# ROOT') < sp.find('# LEAF') and sp.find('# ROOT') > 0)
print('加载文件数:', sp.count('AGENTS.md (from'))
"

cd - && rm -rf "$TMPDIR"
```

**预期输出：**
```
ROOT 在前: True
加载文件数: 2
```

---

### B5 遇到 .git 停止向上递归

```bash
TMPDIR=$(mktemp -d /tmp/ion_b5_XXXX)
mkdir -p "$TMPDIR/workspace/project"
echo "" > "$TMPDIR/workspace/project/.git"
echo "# Project AGENTS" > "$TMPDIR/workspace/project/AGENTS.md"
echo "# OUTSIDE git" > "$TMPDIR/workspace/AGENTS.md"

cd "$TMPDIR/workspace/project"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "git boundary test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
echo "Project: $(grep -c 'Project AGENTS' "$SIDECAR")"  # 预期: 1
echo "OUTSIDE: $(grep -c 'OUTSIDE git' "$SIDECAR")"     # 预期: 0

cd - && rm -rf "$TMPDIR"
```

---

### B6 GEMINI.md 降级

```bash
TMPDIR=$(mktemp -d /tmp/ion_b6_XXXX)
echo "" > "$TMPDIR/.git"
echo "# GEMINI only" > "$TMPDIR/GEMINI.md"

cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "gemini test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
grep -c "GEMINI only" "$SIDECAR"  # 预期: 1

cd - && rm -rf "$TMPDIR"
```

---

### B7 3 文件同目录 → AGENTS 胜出

```bash
TMPDIR=$(mktemp -d /tmp/ion_b7_XXXX)
echo "" > "$TMPDIR/.git"
echo "# AGENTS win" > "$TMPDIR/AGENTS.md"
echo "# CLAUDE lose" > "$TMPDIR/CLAUDE.md"
echo "# GEMINI lose" > "$TMPDIR/GEMINI.md"

cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "three files test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
echo "AGENTS: $(grep -c 'AGENTS win' "$SIDECAR")"    # 预期: 1
echo "CLAUDE: $(grep -c 'CLAUDE lose' "$SIDECAR")"  # 预期: 0
echo "GEMINI: $(grep -c 'GEMINI lose' "$SIDECAR")"  # 预期: 0

cd - && rm -rf "$TMPDIR"
```

---

### B8 三级目录嵌套全加载

```bash
TMPDIR=$(mktemp -d /tmp/ion_b8_XXXX)
echo "" > "$TMPDIR/.git"
echo "# L0 ROOT" > "$TMPDIR/AGENTS.md"
mkdir -p "$TMPDIR/a/b/c"
echo "# L1 MID" > "$TMPDIR/a/b/AGENTS.md"
echo "# L2 LEAF" > "$TMPDIR/a/b/c/AGENTS.md"

cd "$TMPDIR/a/b/c"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "three-level test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
python3 -c "
with open('$SIDECAR') as f: sp = f.read()
p0 = sp.find('L0 ROOT')
p1 = sp.find('L1 MID')
p2 = sp.find('L2 LEAF')
print(f'加载文件数: {sp.count(\"AGENTS.md (from\")}')
print(f'L0 ROOT: {\"L0 ROOT\" in sp}')
print(f'L1 MID: {\"L1 MID\" in sp}')
print(f'L2 LEAF: {\"L2 LEAF\" in sp}')
print(f'排序 ROOT<MID<LEAF: {0 < p0 < p1 < p2}')
"

cd - && rm -rf "$TMPDIR"
```

**预期输出：**
```
加载文件数: 3
L0 ROOT: True
L1 MID: True
L2 LEAF: True
排序 ROOT<MID<LEAF: True
```

---

### B9 .git 是目录（真实 git init）

```bash
TMPDIR=$(mktemp -d /tmp/ion_b9_XXXX)
git init "$TMPDIR"  # 创建 .git 目录（不是文件）
echo "# real git dir" > "$TMPDIR/AGENTS.md"

cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "git-dir test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
grep -c "real git dir" "$SIDECAR"  # 预期: 1

cd - && rm -rf "$TMPDIR"
```

---

### B10 AGENTS.md 为空文件

```bash
TMPDIR=$(mktemp -d /tmp/ion_b10_XXXX)
echo "" > "$TMPDIR/.git"
echo -n "" > "$TMPDIR/AGENTS.md"  # 空文件

cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "empty file test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
# 空文件也加载，有段落头但不 panic
echo "含 project context: $(grep -c 'project context' "$SIDECAR")"  # 预期: >=1
echo "退出码正常: $?"

cd - && rm -rf "$TMPDIR"
```

---

### B11 AGENTS.md 只有 1 行

```bash
TMPDIR=$(mktemp -d /tmp/ion_b11_XXXX)
echo "" > "$TMPDIR/.git"
echo "just one line" > "$TMPDIR/AGENTS.md"

cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "single line test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
grep -c "just one line" "$SIDECAR"  # 预期: 1

cd - && rm -rf "$TMPDIR"
```

---

## 四、Group C：开关 + 边界 + 预算

### C1 `--no-context-files` 关闭注入

```bash
SID=$(basename $(ls -t ~/.ion/agent/sessions/*ion*/*.jsonl | head -1) .jsonl)

# ON
ION_API_KEY=fake ion --session "$SID" --export /tmp/c1_on.html
# OFF
ION_API_KEY=fake ion --no-context-files --session "$SID" --export /tmp/c1_off.html

python3 -c "
import base64, re, json
def extract(path):
    with open(path) as f: h = f.read()
    idx = h.find('eyJ')
    return json.loads(base64.b64decode(re.match(r'[A-Za-z0-9+/=]+', h[idx:idx+300000]).group(0).encode()).decode())
on = extract('/tmp/c1_on.html')
off = extract('/tmp/c1_off.html')
print(f'              ON 模式          OFF 模式')
print(f'prompt 长度:  {len(on[\"systemPrompt\"]):>8}        {len(off[\"systemPrompt\"]):>8}')
print(f'project ctx: {str(\"--- project context ---\" in on[\"systemPrompt\"]):>8}        {str(\"--- project context ---\" in off[\"systemPrompt\"]):>8}')
print(f'contextFiles:{str(on.get(\"contextFiles\",\"无\"))[:20]:>8}  {str(off.get(\"contextFiles\",\"无\")):>8}')
"
```

**预期输出：**
```
              ON 模式          OFF 模式
prompt 长度:     91942           76148
project ctx:     True            False
contextFiles:   ['/Users/...     无
```

---

### C2 `ION_NO_CONTEXT_FILES=1` env 关闭

```bash
SID=$(basename $(ls -t ~/.ion/agent/sessions/*ion*/*.jsonl | head -1) .jsonl)
ION_NO_CONTEXT_FILES=1 ION_API_KEY=fake ion --session "$SID" --export /tmp/c2.html
python3 -c "
import base64, re, json
with open('/tmp/c2.html') as f: h = f.read()
idx = h.find('eyJ')
data = json.loads(base64.b64decode(re.match(r'[A-Za-z0-9+/=]+', h[idx:idx+300000]).group(0).encode()).decode())
print('含 project context:', '--- project context ---' in data['systemPrompt'])
"
# 预期: 含 project context: False
```

---

### C3 超大文件截断（UTF-8 安全）

```bash
ION_CONTEXT_FILES_MAX_CHARS=500 ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "budget test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*ion*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*ion*/${SID}.system-prompt.txt | head -1)

echo "含 truncated: $(grep -c 'truncated' "$SIDECAR")"           # 预期: 1
echo "含 术语规范: $(grep -c '术语规范' "$SIDECAR")"              # 预期: 0
```

---

### C5 CLAUDE.md 优先于 GEMINI.md（无 AGENTS）

```bash
TMPDIR=$(mktemp -d /tmp/ion_c5_XXXX)
echo "" > "$TMPDIR/.git"
echo "# claude wins" > "$TMPDIR/CLAUDE.md"
echo "# gemini loses" > "$TMPDIR/GEMINI.md"

cd "$TMPDIR"
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "claude vs gemini"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*/${SID}.system-prompt.txt | head -1)
echo "claude: $(grep -c 'claude wins' "$SIDECAR")"   # 预期: 1
echo "gemini: $(grep -c 'gemini loses' "$SIDECAR")"  # 预期: 0

cd - && rm -rf "$TMPDIR"
```

---

### C6 大预算（50000）不截断

```bash
ION_CONTEXT_FILES_MAX_CHARS=50000 ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "large budget test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*ion*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*ion*/${SID}.system-prompt.txt | head -1)

echo "含 truncated: $(grep -c 'truncated' "$SIDECAR")"  # 预期: 0
echo "含 术语规范: $(grep -c '术语规范' "$SIDECAR")"     # 预期: 1（完整加载）
```

---

### C7 `--no-context-files` 运行时 sidecar 不含 context

```bash
# 运行时就关闭（不是导出时关闭）
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-context-files --no-session "runtime-off test"

SID=$(basename $(ls -t ~/.ion/agent/sessions/*ion*/*.jsonl | head -1) .jsonl)
SIDECAR=$(ls -t ~/.ion/agent/sessions/*ion*/${SID}.system-prompt.txt | head -1)

echo "含 project context: $(grep -c 'project context' "$SIDECAR")"  # 预期: 0
```

---

## 五、Group D：导出 HTML 面板呈现

### D1 面板显示 Context Files 字段（ON）

```bash
SID=$(basename $(ls -t ~/.ion/agent/sessions/*ion*/*.jsonl | head -1) .jsonl)
ION_API_KEY=fake ion --session "$SID" --export /tmp/d1.html
open /tmp/d1.html
```

**验证点（浏览器）：**
- ✅ 面板显示 `Context Files: AGENTS.md`

### D2 面板显示 none（OFF）

```bash
ION_API_KEY=fake ion --no-context-files --session "$SID" --export /tmp/d2.html
open /tmp/d2.html
```

### D3 system prompt 折叠面板含 AGENTS.md 全文

```bash
open /tmp/d1.html  # 展开 System Prompt 面板
```

---

## 六、Group E：单元测试

```bash
cargo test --lib context_files_extension  # 预期: 23 passed
cargo test --lib rules_engine             # 预期: 28 passed
cargo test --lib export                   # 预期: 28 passed
```

**23 个单元测试清单：**

| # | 测试名 | 覆盖场景 |
|---|--------|---------|
| 1 | test_load_single_agents_md_at_root | git 根有 AGENTS.md |
| 2 | test_walk_upward_finds_root_agents_md | 从深层子目录向上找 |
| 3 | test_priority_agents_before_claude | AGENTS 优先 CLAUDE |
| 4 | test_claude_md_fallback | 只有 CLAUDE |
| 5 | test_multi_level_root_first_ordering | 多级根→叶排序 |
| 6 | test_no_files_returns_empty | 无文件 |
| 7 | test_format_block_empty | 空列表格式化 |
| 8 | test_format_block_contains_marker | 格式化含标记 |
| 9 | test_truncation_respects_char_boundary | UTF-8 截断安全 |
| 10 | test_is_disabled_by_env | 环境变量开关 |
| 11 | test_on_system_prompt_injects | hook 注入 |
| 12 | test_on_system_prompt_noop_when_empty | 无文件不改 prompt |
| 13 | test_three_level_nested_all_loaded | **三级嵌套全加载** |
| 14 | test_empty_agents_md_file | **空文件** |
| 15 | test_single_line_agents_md | **单行文件** |
| 16 | test_claude_before_gemini_priority | **CLAUDE 优先 GEMINI** |
| 17 | test_gemini_md_fallback | **GEMINI 降级** |
| 18 | test_git_directory_not_file | **.git 是目录** |
| 19 | test_stops_at_git_boundary_upward | **.git 边界停止** |
| 20 | test_max_upward_levels_limit | **层级深度限制** |
| 21 | test_large_budget_no_truncation | **大预算不截断** |
| 22 | test_multi_file_shared_budget | **多文件共享预算** |
| 23 | test_on_system_prompt_appends_not_replaces | **追加不替换** |

---

## 七、CI 脚本

```bash
#!/bin/bash
# tests/context_files_ci.sh — Context Files P0 CI 验证
set -e

echo "=== Group E: 单元测试 ==="
cargo test --lib context_files_extension
cargo test --lib rules_engine
cargo test --lib export

echo "=== Group A1: 基础加载 ==="
ION_API_KEY=fake ION_FAUX_REPLY='[{"role":"assistant","content":"ok"}]' \
  ion --no-session "ci-test"
SID=$(basename $(ls -t ~/.ion/agent/sessions/*ion*/*.jsonl | head -1) .jsonl)
ION_API_KEY=fake ion --session "$SID" --export /tmp/ci_a1.html
python3 -c "
import base64, re, json
with open('/tmp/ci_a1.html') as f: h = f.read()
idx = h.find('eyJ')
data = json.loads(base64.b64decode(re.match(r'[A-Za-z0-9+/=]+', h[idx:idx+300000]).group(0).encode()).decode())
assert '--- project context ---' in data['systemPrompt'], 'AGENTS.md not injected'
print('A1 PASS')
"

echo "=== Group C1: 开关 ==="
ION_API_KEY=fake ion --no-context-files --session "$SID" --export /tmp/ci_c1.html
python3 -c "
import base64, re, json
with open('/tmp/ci_c1.html') as f: h = f.read()
idx = h.find('eyJ')
data = json.loads(base64.b64decode(re.match(r'[A-Za-z0-9+/=]+', h[idx:idx+300000]).group(0).encode()).decode())
assert '--- project context ---' not in data['systemPrompt'], 'AGENTS.md not stripped'
print('C1 PASS')
"

echo "=== ALL PASS ==="
```

---

## 八、测试目录结构汇总

| 分类 | 用例数 | 目录/文件结构 |
|------|--------|-------------|
| **主项目** | A1,C1-C7,D1-D3 | `ion/` (git 根, AGENTS.md 53K) |
| **空目录** | A2 | 无任何文件 |
| **B1 子目录递归** | 1 | `root/.git` + `root/AGENTS.md` → 从 `root/src/agent/` 跑 |
| **B2 两文件优先级** | 1 | `.git` + `AGENTS.md` + `CLAUDE.md` |
| **B3 CLAUDE 降级** | 1 | `.git` + `CLAUDE.md` |
| **B4 多级目录** | 1 | `root/.git` + `root/AGENTS.md` + `root/packages/core/AGENTS.md` |
| **B5 .git 边界** | 1 | `workspace/AGENTS.md` + `workspace/project/.git` + `workspace/project/AGENTS.md` |
| **B6 GEMINI 降级** | 1 | `.git` + `GEMINI.md` |
| **B7 三文件同目录** | 1 | `.git` + `AGENTS.md` + `CLAUDE.md` + `GEMINI.md` |
| **B8 三级嵌套** | 1 | `root/.git` + `root/AGENTS.md` + `root/a/b/AGENTS.md` + `root/a/b/c/AGENTS.md` |
| **B9 .git 是目录** | 1 | `git init` 创建的 `.git/` 目录 |
| **B10 空文件** | 1 | `.git` + 空的 `AGENTS.md` |
| **B11 单行文件** | 1 | `.git` + `AGENTS.md`（仅 1 行） |
| **合计** | **26** | 1 主目录 + 12 种虚拟结构 |

## 九、涉及文件

| 文件 | 改动 |
|------|------|
| `src/context_files_extension.rs` | 🆕 扩展实现 + 23 个单元测试 |
| `src/lib.rs` | 模块声明 |
| `src/bin/ion.rs` | cmd_run 注册 + main() `--no-context-files` env 传播 |
| `src/worker_rpc.rs` | worker_rpc 注册（场景 2/3） |
| `src/export.rs` | export 注入 systemPrompt + contextFiles + HTML 面板行 + OFF 剥离 |
