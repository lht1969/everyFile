# Everything-Tauri - 极速本地文件搜索引擎

[![Rust](https://img.shields.io/badge/Rust-1.70+-blue.svg)](https://www.rust-lang.org/)
[![Tauri](https://img.shields.io/badge/Tauri-2.x-purple.svg)](https://tauri.app/)
[![React](https://img.shields.io/badge/React-18.x-61dafb.svg)](https://reactjs.org/)
[![License](https://img.shields.io/badge/License-MIT%2FApache--2.0-green.svg)](LICENSE)

一个基于 Rust + Tauri + React 技术栈构建的极速本地文件搜索引擎，灵感来源于著名的 Everything 软件，提供快速、高效的本地文件搜索体验。

## ✨ 核心特性

### 🔍 智能搜索
- **实时搜索**: 150ms 防抖延迟，输入即搜
- **多关键词搜索**: 支持 AND 逻辑（如 `python 3` 搜索同时包含 python 和 3 的文件）
- **高级搜索语法**:
  - `size:>10MB` - 文件大小过滤（支持 >, <, =, >=, <=）
  - `datemodified:20240401` - 日期过滤（支持 YYYYMMDD/YYYY-MM-DD/YYYY/MM/DD 格式）
  - `path:d:\\test` - 路径过滤
  - `regex:.*\\.py - 正则表达式搜索

### 📁 文件管理
- **智能筛选**: 仅文件/仅文件夹切换
- **路径显示**: 显示目录路径而非完整路径
- **右键操作**: 打开文件、打开文件夹、复制路径、删除文件

### ⚙️ 系统集成
- **系统托盘**: 后台运行，快速访问
- **静默启动**: 支持 `-s` 和 `-S` 命令行参数
- **开机自启**: 管理员模式下自动注册系统启动项
- **多卷支持**: 自动扫描所有可用驱动器

### 📊 数据导出
- **多格式导出**: CSV、TXT、JSON 格式
- **批量导出**: 支持超过 1000 条结果的批量导出
- **北京时区**: 导出文件名使用北京时间

## 🚀 快速开始

### 系统要求
- **操作系统**: Windows 10/11
- **内存**: 至少 2GB RAM
- **磁盘空间**: 至少 100MB 可用空间

### 安装方法

#### 方法一：下载预编译版本（推荐）
1. 访问 [Releases 页面](https://github.com/your-username/everything-tauri/releases)
2. 下载最新版本的 `.exe` 安装包
3. 运行安装程序，按照提示完成安装

#### 方法二：从源码构建
```bash
# 克隆项目
git clone https://github.com/your-username/everything-tauri.git
cd everything-tauri

# 安装依赖
npm install

# 构建应用
npm run tauri build

# 构建完成后，安装包位于 src-tauri/target/release/bundle/
```

## 🎯 使用指南

### 基本搜索
1. 启动应用，程序会自动扫描本地文件系统
2. 在搜索框中输入关键词，如 `python`
3. 搜索结果会实时显示在下方列表中

### 高级搜索示例
```
# 搜索大于 10MB 的 Python 文件
size:>10MB python

# 搜索 2024 年 4 月 1 日修改的文件
datemodified:20240401

# 搜索 D 盘下的所有文件夹
path:d:\\ folders

# 搜索特定扩展名的文件
regex:.*\\.(py|js|ts)$
```

### 键盘快捷键
| 快捷键 | 功能 |
|--------|------|
| `Enter` | 执行搜索 |
| `↑`/`↓` | 上下移动选择项 |
| `Home` | 跳转到列表起始位置 |
| `End` | 跳转到列表结束位置 |
| `PageUp`/`PageDown` | 翻页浏览 |
| `Ctrl+C` | 复制选中文件路径 |

### 右键菜单功能
- **打开**: 直接打开选中的文件
- **打开文件夹**: 在资源管理器中打开文件所在目录
- **复制路径**: 复制文件的完整路径到剪贴板
- **删除**: 删除选中的文件（需确认）

## ⚙️ 配置说明

### 配置文件位置
应用配置存储在以下位置：
- Windows: `%APPDATA%\\everything-tauri\\config.toml`

### 配置选项
```toml
# 扫描设置
[scan]
# 是否扫描隐藏文件
include_hidden_files = false
# 是否扫描系统文件
include_system_files = false
# 是否扫描所有卷
scan_all_volumes = true

# 界面设置
[ui]
# 搜索结果行高
row_height = 24
# 是否显示系统托盘
tray_icon = true

# 搜索设置
[search]
# 每页显示结果数
page_size = 1000
# 搜索时是否区分大小写
case_sensitive = false
```

## 🛠️ 开发指南

### 环境要求
- **Node.js**: 18.x 或更高版本
- **Rust**: 1.70.0 或更高版本
- **Tauri CLI**: `npm install -g @tauri-apps/cli`

### 开发环境搭建
```bash
# 1. 克隆项目
git clone https://github.com/your-username/everything-tauri.git
cd everything-tauri

# 2. 安装前端依赖
npm install

# 3. 开发模式运行
npm run tauri dev
```

### 项目结构
```
everything-tauri/
├── src/                    # React 前端源码
│   ├── components/         # React 组件
│   │   ├── SearchPanel.tsx # 搜索面板组件
│   │   ├── ResultList.tsx  # 结果列表组件
│   │   ├── StatusBar.tsx   # 状态栏组件
│   │   └── SettingsModal.tsx # 设置面板组件
│   ├── App.tsx            # 主应用组件
│   ├── App.css            # 全局样式
│   └── main.tsx           # 入口文件
├── src-tauri/             # Rust 后端源码
│   ├── src/
│   │   ├── commands/      # Tauri 命令模块
│   │   ├── index/         # 文件索引模块
│   │   ├── search/        # 搜索逻辑模块
│   │   ├── fs/            # 文件系统模块
│   │   └── main.rs        # 程序入口
│   ├── Cargo.toml         # Rust 依赖配置
│   └── tauri.conf.json    # Tauri 应用配置
├── package.json           # 前端依赖配置
└── vite.config.ts         # Vite 构建配置
```

### 核心模块说明

#### 搜索查询解析 (`src-tauri/src/search/query.rs`)
```rust
// 支持的高级搜索语法
let query = SearchQuery::parse("size:>10MB datemodified:20240401 python");
// 解析结果包含:
// - keywords: ["python"]
// - size_filter: SizeFilter { operator: GreaterThan, value: 10485760 }
// - date_filter: DateFilter { date_type: Modified, operator: Equal, ... }
```

#### 文件索引管理 (`src-tauri/src/index/monitor.rs`)
```rust
// 卷扫描和索引管理
let mut volume_manager = VolumeManager::new();
volume_manager.add_volume("C:\\\\", is_admin, include_hidden_files, include_system_files);
let (results, total) = volume_manager.search_with_options("query", &options);
```

#### 前端状态管理 (`src/App.tsx`)
```typescript
// React 状态管理
const [results, setResults] = useState<SearchResult[]>([]);
const [searchState, setSearchState] = useState({
  query: '',
  filesOnly: true,
  directoriesOnly: false
});
```

### 构建和打包
```bash
# 开发构建（调试模式）
npm run tauri dev

# 生产构建（发布模式）
npm run tauri build

# 仅构建前端
npm run build

# 仅构建 Rust 后端
cd src-tauri && cargo build --release
```

## 🧪 测试

### 单元测试
```bash
# 运行 Rust 单元测试
cd src-tauri && cargo test

# 运行前端测试
npm test
```

### 集成测试
项目包含以下测试用例：
- 搜索功能测试（关键词、大小、日期、路径过滤）
- 文件操作测试（打开、删除、导出）
- 界面交互测试（键盘导航、右键菜单）
- 性能测试（大规模文件搜索）

## 🤝 贡献指南

我们欢迎各种形式的贡献！请查看以下指南：

### 报告问题
- 使用 [GitHub Issues](https://github.com/your-username/everything-tauri/issues) 报告 bug 或提出功能建议
- 提供详细的复现步骤和系统环境信息

### 提交代码
1. Fork 本仓库
2. 创建功能分支 (`git checkout -b feature/AmazingFeature`)
3. 提交更改 (`git commit -m 'Add some AmazingFeature'`)
4. 推送到分支 (`git push origin feature/AmazingFeature`)
5. 创建 Pull Request

### 代码规范
- **Rust**: 遵循 Rust 官方编码规范
- **TypeScript**: 使用 ESLint + Prettier 规范
- **提交信息**: 使用约定式提交格式
- **文档**: 所有公共 API 需要文档注释

## 📄 许可证

本项目采用双重许可证：
- **MIT License** - 详见 [LICENSE-MIT](LICENSE-MIT)
- **Apache License 2.0** - 详见 [LICENSE-APACHE](LICENSE-APACHE)

您可以选择任一许可证来使用本项目。

## 📞 联系方式

- **项目主页**: [GitHub Repository](https://github.com/your-username/everything-tauri)
- **问题反馈**: [GitHub Issues](https://github.com/your-username/everything-tauri/issues)
- **邮箱**: your.email@example.com

## 🙏 致谢

- 感谢 [Everything](https://www.voidtools.com/) 软件提供的灵感
- 感谢 [Tauri](https://tauri.app/) 团队提供的优秀框架
- 感谢所有贡献者和用户的支持

---

⭐ 如果这个项目对您有帮助，请给个 Star 支持一下！
