# everyFile 项目文档

## 一、项目概述

### 1.1 项目背景

本项目是从 Rust+egui 实现的本地文件搜索引擎 "everyFile" 迁移到 Rust+Tauri+React 技术栈的桌面应用程序。

### 1.2 核心功能

- **快速文件搜索**：支持多种搜索语法（size:, datemodified:, path:, regex:）
- **仅文件/仅目录筛选**：默认仅显示文件
- **分页显示**：每页1000条结果，支持第一页/最后一页/页码跳转
- **键盘导航**：支持 Home、End、PageUp、PageDown、上下箭头
- **鼠标悬停详情**：显示文件名称、大小、修改时间
- **设置面板**：卷管理（显示每个卷的文件数量）、索引优化
- **系统托盘**：后台运行
- **右键菜单**：打开、打开文件夹、复制路径、删除
- **导出功能**：支持 CSV、TXT、JSON 格式，保存对话框
- **实时扫描进度**：每扫描20000个文件更新进度
- **增量更新**：管理员模式下每60秒自动更新索引
- **配置持久化**：保存/加载监控卷配置

### 1.3 技术栈

| 层级 | 技术 |
|------|------|
| 后端 | Rust + Tauri 2.x |
| 前端 | React 18 + TypeScript + Vite |
| UI | CSS 自定义样式 |
| 存储 | TOML 配置文件 |
| Windows API | windows-rs |

---

## 二、项目结构

```
everyFile/
├── src/                          # React 前端源码
│   ├── components/               # React 组件
│   │   ├── SearchPanel.tsx      # 搜索面板
│   │   ├── ResultList.tsx       # 结果列表（含分页、右键菜单）
│   │   ├── StatusBar.tsx        # 状态栏
│   │   └── SettingsModal.tsx    # 设置面板
│   ├── App.tsx                  # 主应用组件
│   ├── App.css                  # 全局样式
│   └── main.tsx                 # 入口文件
│
├── src-tauri/                   # Rust 后端源码
│   ├── src/
│   │   ├── main.rs              # 入口程序
│   │   ├── commands/            # Tauri 命令
│   │   │   ├── search.rs        # 搜索命令
│   │   │   ├── volume.rs        # 卷管理命令
│   │   │   ├── file.rs          # 文件操作命令
│   │   │   ├── export.rs        # 导出命令
│   │   │   ├── config.rs        # 配置命令
│   │   │   └── system.rs        # 系统命令
│   │   ├── index/               # 索引模块
│   │   │   ├── monitor.rs      # 卷扫描监控
│   │   │   ├── mod.rs          # 模块入口
│   │   │   └── database.rs     # 索引数据库
│   │   ├── search/             # 搜索模块
│   │   │   ├── mod.rs          # 搜索结构体
│   │   │   ├── query.rs        # 查询解析
│   │   │   └── mod.rs
│   │   ├── fs/                 # 文件系统模块
│   │   ├── config.rs           # 配置管理
│   │   ├── error.rs            # 错误定义
│   │   └── tray.rs             # 系统托盘
│   ├── Cargo.toml              # Rust 依赖
│   ├── tauri.conf.json         # Tauri 配置
│   └── capabilities/           # 权限配置
│       └── default.json
│
├── package.json                 # 前端依赖
├── vite.config.ts              # Vite 配置
├── tsconfig.json               # TypeScript 配置
└── docs/
    └── PROJECT.md              # 项目文档
```

---

## 三、核心功能实现

### 3.1 卷扫描与索引

**文件位置**：`src-tauri/src/index/monitor.rs`

**功能说明**：
- 使用 walkdir 遍历目录树
- 过滤隐藏文件（以 `.` 开头）
- 过滤回收站（`$Recycle.Bin`）
- 深度限制：10 层
- 无文件数量限制

**关键代码**：
```rust
let walker = walkdir::WalkDir::new(&path)
    .max_depth(10)
    .into_iter()
    .filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        !name.starts_with('.') && !name.eq_ignore_ascii_case("$Recycle.Bin")
    });
```

### 3.2 搜索语法解析

**文件位置**：`src-tauri/src/search/query.rs`

**支持的语法**：
- `size:>100MB` - 大于指定大小
- `size:<50KB` - 小于指定大小
- `datemodified:2024-01-01` - 修改日期
- `path:C:\Users` - 路径包含
- `regex:.*\.txt$` - 正则表达式
- 仅文件名匹配，不匹配路径

### 3.3 前端状态管理

**文件位置**：`src/App.tsx`

**关键状态**：
```typescript
const [results, setResults] = useState<SearchResult[]>([]);
const [pagination, setPagination] = useState({ page: 1, total: 0, total_pages: 0 });
const [searchState, setSearchState] = useState({ query: '', filesOnly: true, directoriesOnly: false });
```

### 3.4 实时扫描进度

**后端事件**：
- `scan-progress`：每扫描20000个文件触发
- `scan-complete`：扫描完成时触发
- `index-updated`：增量更新时触发（管理员模式，每60秒）

**前端监听**：
```typescript
useEffect(() => {
    const unlistenProgress = listen<{ volume: string; count: number }>('scan-progress', (event) => {
        setStatusMessage(`正在扫描${event.payload.volume}卷，已索引 ${event.payload.count} 个文件...`);
    });

    const unlistenComplete = listen<{ volume: string; count: number }>('scan-complete', (event) => {
        setStatusMessage(`${event.payload.volume}卷扫描结束，共 ${event.payload.count} 个文件`);
        loadIndexStatus();
    });

    const unlistenUpdated = listen<{ volume: string; count: number }>('index-updated', (event) => {
        setStatusMessage(`索引更新: ${event.payload.volume}卷新增 ${event.payload.count} 个文件`);
        loadIndexStatus();
    });

    return () => {
      unlistenProgress.then(fn => fn());
      unlistenComplete.then(fn => fn());
      unlistenUpdated.then(fn => fn());
    };
}, []);
```

### 3.5 增量更新机制

**文件位置**：`src-tauri/src/main.rs`

**实现逻辑**：
- 检测管理员权限（`fs::is_elevated()`）
- 管理员模式下启动后台任务
- 每60秒重新扫描所有已监控卷
- 发送 `index-updated` 事件通知前端

---

## 四、配置文件

**位置**：`%APPDATA%/everyFile-tauri/config.toml`

```toml
scan_all_volumes = true
default_volume = "D:"
max_cache_items = 50
max_history_items = 20
monitored_volumes = ["C:", "D:", "E:"]

[index_settings]
enable_usn_journal = false
include_hidden_files = false
include_system_files = false
update_interval = 300
```

---

## 五、功能列表

### 5.1 完成的功能

| 功能 | 状态 | 备注 |
|------|------|------|
| 搜索语法支持 | ✅ | size:, datemodified:, path:, regex: |
| 仅文件/仅目录 | ✅ | 默认仅文件 |
| 分页显示 | ✅ | 每页1000条 |
| 键盘导航 | ✅ | Home/End/PageUp/PageDown/箭头 |
| 鼠标悬停详情 | ✅ | 显示文件信息 |
| 设置面板 | ✅ | 卷管理、索引优化 |
| 系统托盘 | ✅ | 后台运行 |
| 右键菜单 | ✅ | 打开/打开文件夹/复制路径/删除 |
| 导出功能 | ✅ | CSV/TXT/JSON，保存对话框，导出全部结果 |
| 扫描进度显示 | ✅ | 实时显示进度，每20000条更新 |
| 卷文件数量 | ✅ | 设置页显示每个卷的索引数量 |
| 移除10万限制 | ✅ | 无限制扫描 |
| 跳过回收站 | ✅ | 过滤 $Recycle.Bin |
| 翻页保持搜索条件 | ✅ | 记住搜索词和筛选条件 |
| 配置保存 | ✅ | 保存/加载监控卷配置 |
| 增量更新 | ✅ | 管理员模式每60秒更新 |
| 第一页/最后一页 | ✅ | 分页按钮 |
| 页码跳转 | ✅ | 输入框直接跳转 |
| 状态栏刷新 | ✅ | 每5秒刷新索引状态 |
| 搜索框修复 | ✅ | 支持输入和粘帖 |

### 5.2 已知问题

- 首次启动需要管理员权限才能扫描所有卷
- 扫描大卷（如C盘）可能需要较长时间
- 增量更新是全量扫描，非真正的USN Journal

---

## 六、构建与运行

### 6.1 开发模式

```bash
# 前端
npm run dev

# 后端
cargo build
```

### 6.2 生产构建

```bash
# 前端
npm run build

# Tauri
cargo build --release
```

### 6.3 运行

```bash
# 开发运行
npm run tauri dev

# 生产运行
npm run tauri build
```

---

## 七、依赖版本

| 依赖 | 版本 |
|------|------|
| Tauri | 2.x |
| React | 18.3.1 |
| TypeScript | 5.7.3 |
| Vite | 6.x |
| walkdir | 2 |
| windows-rs | 0.52 |
| serde | 1 |
| chrono | 0.4 |
| tokio | 1.35 |

---

## 八、事件列表

| 事件名 | 触发时机 | 数据格式 |
|--------|----------|----------|
| scan-progress | 每扫描20000个文件 | `{ volume: string, count: number }` |
| scan-complete | 扫描完成 | `{ volume: string, count: number }` |
| index-updated | 增量更新 | `{ volume: string, count: number }` |

---

## 九、Tauri 命令

| 命令 | 功能 |
|------|------|
| search_files | 搜索文件 |
| get_search_suggestions | 获取搜索建议 |
| get_volumes | 获取可用卷 |
| add_volume | 添加监控卷 |
| remove_volume | 移除监控卷 |
| refresh_volumes | 刷新卷列表 |
| rebuild_index | 重建索引 |
| optimize_index | 优化索引 |
| get_index_status | 获取索引状态 |
| get_monitored_volumes | 获取已监控卷（含文件数量） |
| open_file | 打开文件 |
| open_folder | 打开所在文件夹 |
| delete_file | 删除文件 |
| export_csv | 导出CSV |
| export_txt | 导出TXT |
| export_json | 导出JSON |
| export_all_results | 导出全部结果 |
| get_config | 获取配置 |
| save_config | 保存配置 |
| is_admin | 检查管理员权限 |

---

*文档更新时间：2026-04-07*