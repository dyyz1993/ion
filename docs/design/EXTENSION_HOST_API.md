# Extension Host API — ctx.fs 统一文件访问 设计文档

> **状态：待定** — 给扩展（含 WASM）提供统一的、经过权限控制的文件系统访问能力。
>
> 对齐 pi 的 `ExtensionContext.fs`（FileSystemCapability）。

---

## 何时使用这个文档

- WASM 扩展想读项目文件但被沙箱挡住
- 扩展想让文件操作走 Runtime 路由（本地/沙箱/远程透明）
- 想给扩展提供自动创建的 4 级数据目录

**前置阅读**：[EXTENSION_SYSTEM.md](./EXTENSION_SYSTEM.md)、[CONFIG_DIMENSIONS.md](./CONFIG_DIMENSIONS.md)

---

## 1. 问题

ION 扩展当前的文件访问：

| 扩展类型 | 怎么访问文件 | 问题 |
|---------|------------|------|
| 内置 Rust 扩展（Memory/Bash/FileSnapshot） | 直接 `std::fs::read_to_string` | 有特权，不走权限/路由 |
| WASM 扩展 | **不能读项目文件**（沙箱隔离，只有 4 维数据存储目录） | 看不了项目的 docs/src/config |
| HookExtension | 直接 `std::fs` 或 spawn bash | 不走 Runtime 路由 |

**缺失**：扩展没有统一的、受控的文件访问 API。

pi 的解决方案是 `ExtensionContext.fs`：一个 `FileSystemCapability` trait，本地/远程/沙箱透明，带权限检查。

## 2. 设计

### 2.1 FileSystemCapability trait

**文件**：`src/agent/extension.rs`（Extension trait 旁边新增）

```rust
/// 文件系统能力——扩展通过它访问文件（而不是裸 std::fs）
/// 走 Runtime 路由，受 PermissionEngine 管控
pub trait FileSystemCapability: Send + Sync {
    /// 读文件
    fn read_file(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;

    /// 写文件
    fn write_file(&self, path: &str, content: &str) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;

    /// 列目录
    fn list_dir(&self, path: &str) -> Pin<Box<dyn Future<Output = Result<Vec<DirEntry>, String>> + Send + '_>>;

    /// 文件是否存在
    fn path_exists(&self, path: &str) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;

    /// glob 匹配（简化版）
    fn glob(&self, pattern: &str) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + '_>>;
}

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}
```

### 2.2 实现：RuntimeFileSystem

**文件**：`src/agent/extension.rs`

```rust
/// 基于 Runtime 的 FileSystemCapability 实现
/// 把文件操作委托给 Runtime（走本地/沙箱/远程路由）
pub struct RuntimeFileSystem {
    runtime: Arc<dyn crate::runtime::Runtime>,
    /// 允许访问的根目录白名单（防逃逸）
    allowed_roots: Vec<PathBuf>,
}

impl FileSystemCapability for RuntimeFileSystem {
    async fn read_file(&self, path: &str) -> Result<String, String> {
        // 1. 路径安全检查（safe_join，防 ../../../ 逃逸）
        let safe_path = self.safe_join(path)?;
        // 2. 权限检查（走 PermissionEngine）
        // self.permission.check("file.read", &safe_path)?;
        // 3. 委托 Runtime（自动走 local/sandbox/remote 路由）
        self.runtime.read_file(&safe_path).await
    }
    // ... write_file / list_dir / glob 类似
}

impl RuntimeFileSystem {
    /// 路径安全检查：确保 path 在 allowed_roots 之一下面
    fn safe_join(&self, path: &str) -> Result<String, String> {
        let p = std::path::Path::new(path);
        let canonical = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.allowed_roots[0].join(p)
        };
        // 检查 canonical 在 allowed_roots 之下
        for root in &self.allowed_roots {
            if canonical.starts_with(root) {
                return Ok(canonical.to_string_lossy().to_string());
            }
        }
        Err(format!("path '{}' outside allowed roots", path))
    }
}
```

### 2.3 默认 allowed_roots

```rust
fn default_allowed_roots(project_root: &Path) -> Vec<PathBuf> {
    vec![
        project_root.to_path_buf(),           // 项目根目录
        crate::paths::root(),                  // ~/.ion/
    ]
}
```

用户可在 settings.json 配额外的允许目录：
```json
{
  "extension_fs": {
    "allowed_roots": ["/tmp", "/var/log"]
  }
}
```

### 2.4 扩展如何拿到 ctx.fs

**内置 Rust 扩展**（通过 Extension trait 的上下文）：

给 Extension trait 的钩子方法加一个可选的 `fs: Option<&dyn FileSystemCapability>` 参数。但这会改所有钩子签名（影响面大）。

**更轻量的方案**——不改 trait，而是给 ExtensionRegistry 持有一个 `Arc<FileSystemCapability>`，扩展通过 registry 拿：

```rust
// ExtensionRegistry 新增
pub struct ExtensionRegistry {
    extensions: Vec<Box<dyn Extension>>,
    // ... 现有字段
    fs: Option<Arc<dyn FileSystemCapability>>,  // 新增
}

impl ExtensionRegistry {
    pub fn filesystem(&self) -> Option<&Arc<dyn FileSystemCapability>> {
        self.fs.as_ref()
    }
}
```

扩展在钩子里通过 `registry.filesystem()` 拿到 ctx.fs。

**WASM 扩展**——新增宿主函数：

```rust
// src/wasm_extension.rs 新增宿主函数
fn host_read_file(ctx: &FunctionEnvMut<WasmEnv>, path_ptr: u32, path_len: u32) -> u32 {
    // 1. 从 WASM 内存读 path
    // 2. 调 ctx.fs.read_file(path)
    // 3. 把结果写回 WASM 内存
}

fn host_list_dir(ctx: &FunctionEnvMut<WasmEnv>, path_ptr: u32, path_len: u32) -> u32 {
    // 类似
}
```

### 2.5 4 级数据目录（对齐 pi）

复用已有的 storage_context（4 维存储），给扩展提供自动创建的数据目录：

```rust
pub struct ExtensionDataDirs {
    /// 会话级：~/.ion/agent/sessions/<project>/data/<sessionId>/<ext>/
    pub session: PathBuf,
    /// CWD 级：~/.ion/agent/cwd-data/<encoded-cwd>/<ext>/
    pub cwd: PathBuf,
    /// 项目级：~/.ion/agent/project-data/<encoded-project>/<ext>/
    pub project: PathBuf,
    /// 全局级：~/.ion/agent/extensions-data/<ext>/
    pub global: PathBuf,
}
```

扩展通过 registry 拿到自己的 `ExtensionDataDirs`（按扩展名隔离）。

## 3. 改动文件清单

| 文件 | 改动 | 行数 |
|------|------|------|
| `src/agent/extension.rs` | FileSystemCapability trait + RuntimeFileSystem + ExtensionRegistry.fs | ~120 |
| `src/wasm_extension.rs` | host_read_file / host_list_dir 宿主函数 | ~80 |
| `src/bin/ion_worker.rs` | 构造 RuntimeFileSystem 并注入 registry | ~20 |
| `tests/extension_fs_ci.sh` | CLI 测试 | ~60 |
| **总计** | | **~280** |

## 4. CLI 测试指南

### Group A：内置扩展通过 ctx.fs 读文件

```bash
# A1 写一个测试扩展，用 registry.filesystem() 读 package.json
# FauxProvider 驱动
ion rpc --session <sid> --method prompt --params '{"text":"读 package.json 的 name 字段"}'
# 验证扩展读到了内容
```

### Group B：WASM 扩展通过 host_read_file 读文件

```bash
# B1 写一个 WASM 扩展调 host_read_file 读 README.md
# 编译 .wasm → 加载 → 验证能读到内容
```

### Group C：路径安全（逃逸防护）

```bash
# C1 尝试读 allowed_roots 之外的文件
# 验证返回错误"outside allowed roots"
```

## 5. 并行开发注意事项

- **不依赖**其他 3 份文档，可独立并行开发
- 改动集中在 `extension.rs`（新 trait + 实现）+ `wasm_extension.rs`（新宿主函数）+ `ion_worker.rs`（注入）
- 与 PERMISSION_STORE.md 有轻微交叉（都碰 permission），但改的函数不同，不冲突
- 与 SKILL_TOOL.md 完全不重叠
- **注意**：改 ExtensionRegistry 结构会影响所有扩展注册，编译时注意——但只加了 Option 字段，向后兼容

## 6. 对标 pi

| 对比项 | pi | ION |
|--------|-----|-----|
| ctx.fs | ✅ FileSystemCapability trait | 🔧 本文档新增 |
| 路由 | 本地/远程透明 | Runtime trait（对齐，更强——多了沙箱/容器） |
| 权限 | 走 PermissionProvider | 走 PermissionEngine（对齐） |
| 路径安全 | 有 | safe_join（对齐） |
| 4 级数据目录 | ✅ ExtensionContext | ✅ StorageContext（已有，需暴露给扩展） |
| WASM 文件访问 | N/A（pi 无 WASM） | 🔧 本文档新增（ION 独有需求） |
