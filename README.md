# everyFile

> 基于 Rust + Tauri 构建的极速本地文件搜索引擎，采用 NTFS USN Journal 实时索引，可在毫秒级搜索海量文件。

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#许可证)
[![Tauri](https://img.shields.io/badge/Tauri-v2-orange)](https://tauri.app/)
[![Platform](https://img.shields.io/badge/platform-Windows-blue)](#系统要求)

## 目录

- [项目简介](#项目简介)
- [核心特性](#核心特性)
- [系统要求](#系统要求)
- [安装与构建](#安装与构建)
- [快速开始](#快速开始)
- [使用指南](#使用指南)
- [配置说明](#配置说明)
- [项目结构](#项目结构)
- [技术栈](#技术栈)
- [性能表现](#性能表现)
- [常见问题](#常见问题)
- [许可证](#许可证)

## 项目简介

everyFile 是一款 Windows 平台下的本地文件搜索工具，灵感来源于 [everyFile Search Engine](https://www.voidtools.com/)。它使用 Tauri v2 框架构建，后端采用 Rust 语言编写，前端使用 React + TypeScript，结合 NTFS USN Journal 实现文件系统的实时索引与毫秒级搜索。

与传统的文件遍历搜索不同，本项目直接读取 NTFS 主文件表（MFT）建立索引，并通过 USN Journal 进行增量更新，无需逐目录扫描即可在百万级文件量下保持秒级响应。

## 核心特性

- **极速索引**：直接读取 NTFS MFT，秒级完成全量索引构建（200 万文件约 30 秒）
- **实时更新**：通过 USN Journal 监控文件变更，文件增删改在 5 秒内同步到搜索结果
- **海量文件支持**：内存优化设计，可稳定承载 200 万+ 文件索引
- **虚拟滚动**：前端虚拟滚动列表，仅渲染可见行，流畅浏览百万级结果
- **多卷支持**：同时索引多个 NTFS / FAT32 / exFAT 卷
- **高级查询语法**：支持关键词、通配符、正则、路径过滤、大小/日期过滤
- **多排序字段**：按名称 / 路径 / 大小 / 修改时间排序，升降序自由切换
- **结果导出**：支持 CSV / TXT / JSON 格式导出搜索结果
- **系统集成**：系统托盘常驻、开机自启、Windows Toast 通知
- **管理员/普通用户双模式**：管理员模式启用 USN Journal 实时索引；普通用户使用 walkdir 兜底扫描
- **轻量安装**：基于 Tauri v2，安装包仅约 10MB，内存占用低

## 系统要求

- **操作系统**：Windows 10 1809+ / Windows 11（64 位）
- **运行时**：WebView2 Runtime（Windows 11 已内置；Windows 10 可能需安装）
- **推荐配置**：
  - 8GB 以上内存
  - SSD 存储（显著加快 MFT 读取速度）
  - 多核 CPU（MFT 解析与搜索并行加速）

> **管理员权限说明**：以管理员身份运行可启用 NTFS USN Journal 实时索引，获得最佳性能；普通用户模式下应用退化为定时 walkdir 全量扫描，仍可使用但更新延迟较长。

## 安装与构建

### 方式一：下载预编译版本（普通用户推荐）

前往 [Releases 页面](#)下载最新版 `everyFile_x.x.x_x64-setup.exe`，双击安装即可。

### 方式二：从源码构建（开发者）

#### 前置依赖

- [Rust](https://www.rust-lang.org/) 1.75+（含 cargo）
- [Node.js](https://nodejs.org/) 18+（含 npm）
- [Microsoft Visual C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)（含 Windows SDK）
- [Tauri CLI prerequisites](https://v2.tauri.app/start/prerequisites/)

#### 构建步骤

```powershell
# 1. 克隆仓库
git clone <仓库地址> everyFile
cd everyFile

# 2. 安装前端依赖
npm install

# 3. 开发模式运行（带热重载）
npm run tauri dev

# 4. 生产构建（生成 NSIS 安装包）
npm run tauri build
```

构建产物位于 `src-tauri/target/release/bundle/nsis/` 目录下。

## 快速开始

1. **首次启动**：双击 `everyFile.exe`，应用自动开始扫描所有 NTFS 卷
2. **搜索文件**：在搜索框输入关键词（如 `report`），结果实时显示
3. **打开文件**：双击结果行直接打开；右键调用 Windows 原生上下文菜单
4. **排序**：点击列头切换排序字段与方向
5. **关闭窗口**：点击关闭按钮即最小化到系统托盘，右键托盘图标可退出

## 使用指南

### 搜索语法

搜索框支持以下查询语法（空格分隔多个条件，全部匹配才会命中）：

| 语法 | 示例 | 说明 |
|------|------|------|
| 关键词 | `report` | 文件名包含 "report" |
| 通配符 | `*.pdf` | glob 模式匹配 |
| 正则表达式 | `regex:^report_\d+` | 正则匹配文件名 |
| 路径过滤 | `path:C:\Users` | 路径包含指定字符串 |
| 仅文件夹 | `:folder` 或 `:folders` | 仅匹配目录 |
| 大小过滤 | `size:>100MB` | 大于 100MB 的文件 |
| 大小过滤 | `size:<1KB` | 小于 1KB 的文件 |
| 修改日期 | `datemodified:today` | 今天修改的文件 |
| 修改日期 | `dm:2024-01-01` | 指定日期修改的文件 |
| 修改日期 | `dm:>2024-01-01` | 2024 年后修改的文件 |

**复合查询示例**：

```
report path:D:\Work size:>1MB datemodified:today
```

含义：在 `D:\Work` 路径下，搜索文件名含 `report`、大小大于 1MB、今日修改的文件。

### 快捷键

| 快捷键 | 功能 |
|--------|------|
| `↑` / `↓` | 上下移动选中行 |
| `Home` / `End` | 跳到列表开头 / 末尾 |
| `PageUp` / `PageDown` | 翻页 |
| `Enter` | 打开选中文件 |
| `Esc` | 关闭窗口（焦点不在输入框时） |

### 导出搜索结果

点击搜索栏右侧"导出"按钮，选择格式（CSV / TXT / JSON），指定保存路径即可。导出范围为当前查询条件下的全部结果。

## 配置说明

配置文件位于 `%APPDATA%\everyFile\config.toml`，使用 TOML 格式：

```toml
default_volume = "D:"
max_cache_items = 50
max_history_items = 20
monitored_volumes = ["D:", "C:"]
startup = false

[index_settings]
enable_usn_journal = true
include_hidden_files = false
include_system_files = false
update_interval = 5
```

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `default_volume` | string | `"D:"` | 默认卷 |
| `max_cache_items` | usize | `50` | 最大缓存条目数 |
| `max_history_items` | usize | `20` | 搜索历史最大保存数 |
| `monitored_volumes` | array | `["D:"]` | 监控的卷盘符列表 |
| `startup` | bool | `false` | 是否开机自启 |
| `index_settings.enable_usn_journal` | bool | `true` | 启用 USN Journal |
| `index_settings.include_hidden_files` | bool | `false` | 是否包含隐藏文件 |
| `index_settings.include_system_files` | bool | `false` | 是否包含系统文件 |
| `index_settings.update_interval` | u32 | `5` | USN 轮询间隔（秒） |

也可通过设置界面修改配置，更改后立即生效。

## 项目结构

```
everyFile/
├── src/                              # 前端源码（React + TypeScript）
│   ├── components/                   # UI 组件
│   │   ├── SearchPanel.tsx           # 搜索输入栏
│   │   ├── ResultList.tsx            # 虚拟滚动结果列表
│   │   ├── StatusBar.tsx             # 底部状态栏
│   │   └── SettingsModal.tsx         # 设置弹窗
│   ├── hooks/                        # 自定义 Hooks
│   │   ├── useVirtualScroll.ts       # 虚拟滚动实现
│   │   ├── useFileIcon.ts            # 文件图标获取
│   │   └── useColumnWidths.ts        # 列宽管理
│   ├── types/                        # TypeScript 类型定义
│   ├── utils/                        # 工具函数
│   │   ├── format.ts                 # 大小/时间格式化
│   │   └── highlight.tsx             # 关键词高亮
│   ├── App.tsx                       # 应用根组件
│   ├── main.tsx                      # 入口文件
│   ├── App.css / index.css           # 样式
│
├── src-tauri/                        # 后端源码（Rust）
│   ├── src/
│   │   ├── commands/                 # Tauri 命令（前端可调用）
│   │   │   ├── search.rs             # 搜索命令
│   │   │   ├── volume.rs             # 卷管理命令
│   │   │   ├── file.rs               # 文件操作命令
│   │   │   ├── export.rs             # 导出命令
│   │   │   ├── icon.rs               # 图标获取命令
│   │   │   ├── shell_menu.rs         # 右键菜单命令
│   │   │   ├── system.rs             # 系统命令（管理员/自启）
│   │   │   └── config.rs             # 配置命令
│   │   ├── index/                    # 索引核心模块
│   │   │   ├── monitor.rs            # 卷监控与搜索缓存
│   │   │   ├── ntfs_mft.rs           # NTFS MFT 解析
│   │   │   ├── usn_worker.rs         # USN Journal worker
│   │   │   ├── usn_types.rs          # USN 类型定义
│   │   │   ├── path_table.rs         # 路径前缀压缩表
│   │   │   ├── database.rs           # SQLite 数据库
│   │   │   ├── scanner.rs            # walkdir 扫描器
│   │   │   └── lib/                  # MFT 底层库
│   │   ├── search/                   # 搜索模块
│   │   │   ├── mod.rs                # 数据结构与接口
│   │   │   └── query.rs              # 查询解析器
│   │   ├── fs/mod.rs                 # 文件系统操作
│   │   ├── config.rs                 # 配置管理
│   │   ├── error.rs                  # 错误类型
│   │   ├── tray.rs                   # 系统托盘
│   │   ├── tray_notification.rs      # Toast 通知
│   │   ├── file_logger.rs            # 文件日志
│   │   ├── main.rs                   # 程序入口
│   │   └── lib.rs                    # 库入口
│   ├── Cargo.toml                    # Rust 依赖配置
│   ├── tauri.conf.json               # Tauri 应用配置
│   └── build.rs                      # 构建脚本
│
├── index.html                        # 前端入口 HTML
├── package.json                      # 前端依赖配置
├── vite.config.ts                    # Vite 构建配置
├── tsconfig.json                     # TypeScript 配置
└── README.md                         # 本文件
```

## 技术栈

### 后端（Rust）

- **[Tauri v2](https://v2.tauri.app/)**：跨平台桌面应用框架，提供 IPC、托盘、通知等能力
- **[tokio](https://tokio.rs/)**：异步运行时，管理扫描/轮询/合并等异步任务
- **[rayon](https://docs.rs/rayon/)**：数据并行库，用于 MFT 解析与搜索的并行加速
- **[parking_lot](https://docs.rs/parking_lot/)**：高性能同步原语
- **[windows](https://docs.rs/windows/)**：Windows API 绑定，调用 MFT / USN / Shell 等原生接口
- **[usn-journal-rs](https://docs.rs/usn-journal-rs/)**：USN Journal 操作库
- **[ntfs](https://docs.rs/ntfs/)**：NTFS 文件系统解析库
- **[compact_str](https://docs.rs/compact_str/)**：紧凑字符串，减少内存分配
- **[bitvec](https://docs.rs/bitvec/)**：位图库，用于 file_id 索引
- **[crossbeam-channel](https://docs.rs/crossbeam-channel/)**：多生产者多消费者通道
- **[rusqlite](https://docs.rs/rusqlite/)**：SQLite 绑定，持久化 USN 状态
- **[serde](https://serde.rs/)** / **[serde_json](https://docs.rs/serde_json/)**：序列化框架
- **[chrono](https://docs.rs/chrono/)**：日期时间处理
- **[regex](https://docs.rs/regex/)** / **[glob](https://docs.rs/glob/)**：查询模式匹配

### 前端（TypeScript）

- **[React 18](https://react.dev/)**：UI 框架
- **[TypeScript 5](https://www.typescriptlang.org/)**：类型安全
- **[Vite 6](https://vitejs.dev/)**：构建工具，支持热更新
- **[@tauri-apps/api](https://v2.tauri.app/reference/javascript/)**：Tauri 前端 API
- **[@tauri-apps/plugin-dialog](https://v2.tauri.app/plugin/dialog/)**：原生对话框

## 性能表现

以下数据基于典型场景（Windows 11 + SSD + 200 万文件）：

| 指标 | 管理员模式（USN） | 普通用户模式（walkdir） |
|------|-------------------|-------------------------|
| 全量索引构建 | ~30 秒 | ~3-5 分钟 |
| 文件变更同步延迟 | 5 秒（轮询间隔） | 300 秒（默认间隔） |
| 普通关键词搜索 | <100ms | <100ms |
| 空查询返回前 50 条 | <50ms | <50ms |
| 内存占用（200 万文件） | ~250 MB | ~250 MB |
| 安装包大小 | ~10 MB | ~10 MB |

### 性能优化要点

- **内存压缩**：`FileEntry` 使用 `path_id` 替代完整路径，节省约 74% 内存
- **路径前缀压缩**：`PathTable` 共享父目录路径，200 万文件仅需 40 MB
- **位图索引**：`bitvec` 替代 `Vec<u32>`，file_id 索引节省 8.5 MB
- **排序缓存**：按需缓存排序索引，LRU 驱逐策略限制内存上限
- **增量合并**：delta 缓存超 1000 条或 10MB 时自动合并回 base
- **虚拟滚动**：仅渲染可见行 + overscan，百万级列表流畅滚动

更多性能细节请参阅 [技术文档](./docs/TECHNICAL.md)。

## 常见问题

### Q1：为什么搜索不到刚创建的文件？

A：管理员模式下 USN Journal 轮询间隔为 5 秒，文件变更最多 5 秒后才会出现在搜索结果中。可在设置中调整轮询间隔（不建议小于 5 秒，会增加 CPU 占用）。

### Q2：非管理员模式为什么更新很慢？

A：非管理员无法访问 USN Journal，应用退化为 walkdir 定时全量扫描，默认间隔 300 秒。建议以管理员身份运行获得最佳体验。

### Q3：为什么有些卷不被索引？

A：默认仅索引配置文件中 `monitored_volumes` 列出的卷。可在设置界面添加/移除监控卷。

### Q4：日志文件在哪里？

A：日志位于 `%APPDATA%\everyFile\logs\everyFile-YYYY-MM-DD.log`，按天滚动，单文件最大 5MB，最多保留 7 个。

### Q5：如何开机自启？

A：在设置界面勾选"开机自启"，应用会写入注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，并使用 `-s` 参数静默启动（启动后显示 Toast 通知，主窗口隐藏到托盘）。

### Q6：为什么搜索结果与资源管理器不一致？

A：可能原因：
- 文件刚创建/删除，尚未被 USN 检测到（等待 5 秒）
- 配置中关闭了"包含隐藏文件"或"包含系统文件"
- 非管理员模式下增量扫描间隔较长

### Q7：如何彻底退出？

A：点击窗口关闭按钮只会最小化到托盘。要彻底退出，请右键托盘图标选择"退出"。

## 许可证

本项目采用 **MIT OR Apache-2.0** 双许可证，您可以选择其中之一使用。

- [MIT License](https://opensource.org/licenses/MIT)
- [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0)

---

作者：lht1969

版本：1.0.0

如果本项目对您有帮助，欢迎 Star 支持！
