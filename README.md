# Everything-Tauri — 极速本地文件搜索引擎

基于 Rust + Tauri 2 + React 18 构建的 Windows 本地文件搜索引擎，灵感来自 Everything。

## 安装

### 系统要求
- Windows 10/11
- 内存 ≥ 2GB

### 下载
从 [Releases 页面](https://github.com/your-username/everything-tauri/releases) 下载最新 `.exe` 安装包运行即可。

### 从源码构建
```bash
npm install
npm run tauri build
# 安装包位于 src-tauri/target/release/bundle/
```

## 快速上手

启动后程序会自动扫描本地磁盘，在搜索框中输入关键词即可实时搜索文件名。

```
# 基本搜索 — 直接输入关键词
python
项目报告

# 多个关键词同时匹配（AND 逻辑）
python 3
report 2024
```

## 搜索语法详解

### 通配符搜索（Glob）

搜索框直接输入带 `*` `?` 的词即可使用通配符，无需加任何前缀。

| 写法 | 匹配结果 |
|------|---------|
| `chs*` | 所有以 chs 开头的文件 |
| `*.rs` | 所有 Rust 源文件 |
| `pic?.jpg` | pic1.jpg、pica.jpg、pic_.jpg 等 |
| `[ab]*.txt` | 以 a 或 b 开头的 txt 文件 |

> 不含通配符的关键词仍然按子串匹配，`python` 会匹配 python3.exe、jpython.zip 等。

### 大小搜索

`size:` 后跟操作符和数值。

| 示例 | 含义 |
|------|------|
| `size:>1GB` | 大于 1GB |
| `size:<500KB` | 小于 500KB |
| `size:=10MB` | 恰好 10MB |
| `size:100MB` | 不写操作符时默认 ≥（大于或等于） |

支持的单位：`GB` `MB` `KB` `B`，大小写均可（`MB` `mb` `Mb` 都行）。

### 日期搜索

按修改/创建/访问日期搜索，支持完整名称和缩写。

| 完整名 | 缩写 | 含义 |
|--------|------|------|
| `datemodified:` | `dm:` | 修改日期 |
| `datecreated:` | `dc:` | 创建日期 |
| `dateaccessed:` | `da:` | 访问日期 |

日期格式：`YYYY/MM/DD` `YYYY-MM-DD` `YYYYMMDD`

特殊值：`today`（今天）、`yesterday`（昨天）

```
dm:=2026/07/06          # 修改日期等于 2026 年 7 月 6 日
dc:>=2024-01-01         # 创建日期在 2024 年 1 月 1 日之后
da:<today               # 访问日期在今天之前
datemodified:>=today   # 今天及之后修改的文件
```

### 路径搜索

`path:` 按路径过滤，不区分大小写。

```
path:Downloads                  # 路径中包含 Downloads 的文件
path:C:\Users\Local :folder     # 仅匹配 C:\Users\Local 路径下的文件夹
EBWebView :folder path:C:\Users # 仅匹配 C:\Users 下名为 EBWebView 的文件夹
```

### 正则搜索

`regex:` 后跟正则表达式，匹配文件名。

```
regex:^\d{4}-.*\.txt$   # 以 4 位数字开头的 txt 文件
regex:\.(jpg|png|gif)$  # 图片文件
```

### 组合使用

多种条件用空格分隔，所有条件必须同时满足。

```
report size:>1MB dm:>=2026/01/01    # 1 月后的 >1MB 的报告
*.jpg size:<500KB path:C:\Photos    # C:\Photos 下小于 500KB 的 jpg
```

## 设置

点击齿轮按钮 ⚙ 打开设置面板。

### 卷管理
添加或移除监控的磁盘卷，每个卷会显示已索引的文件数量。

### 索引设置
- **包含隐藏文件** — 是否索引隐藏属性和以点开头的文件
- **包含系统文件** — 是否索引系统属性的文件（如 System Volume Information）
- **扫描所有卷** — 启动时自动检测并添加所有 NTFS 卷

### 系统设置
- **开机启动** — 开启后随系统自动启动（静默模式运行于后台）

### 索引管理
- **后台定时重建** — 非管理员用户可设置定期重建索引（默认 5 分钟），设为 0 关闭
- **立即重建索引** — 随时手动重建

## 文件操作

### 右键菜单
- **打开** — 用默认程序打开文件
- **打开文件夹** — 在资源管理器中定位文件
- **复制路径** — 复制完整路径到剪贴板
- **删除** — 永久删除文件（需确认）

### 键盘快捷键

| 快捷键 | 功能 |
|--------|------|
| `↑` `↓` | 上下选择 |
| `Home` `End` | 跳到列表首尾 |
| `PageUp` `PageDown` | 翻页 |
| `Enter` | 打开选中文件 |
| `Escape` | 清空搜索框 |

### 导出
搜索后点击导出下拉菜单，可选择 CSV / TXT / JSON 格式，通过系统保存对话框选择导出位置。

## 管理员 vs 普通用户

| 功能 | 管理员 | 普通用户 |
|------|--------|---------|
| 卷访问 | 所有 NTFS 卷 | 可访问的卷 |
| 后台更新 | 每 120 秒增量扫描 | 前台定时重建（可设置间隔） |
| 开机启动 | ✓ | ✓ |

## 数据文件位置

| 文件 | 路径 |
|------|------|
| 配置文件 | `%APPDATA%\Everything\config.toml` |
| 索引数据库 | `%APPDATA%\Everything\everything.db` |
| 运行日志 | `%APPDATA%\Everything\logs\everything-YYYY-MM-DD.log` |

## 命令行参数

| 参数 | 效果 |
|------|------|
| `-s` 或 `-S` | 静默启动，无窗口运行 |

静默模式下启动后会弹出 Windows Toast 通知提示程序已在后台运行，点击任务栏托盘图标可打开主窗口。

## 技术栈

| 层 | 技术 |
|----|------|
| 桌面框架 | Tauri 2.x |
| 前端 | React 18, TypeScript, Vite 6 |
| 后端 | Rust, SQLite (WAL 模式) |
| 文件遍历 | walkdir |
| 搜索解析 | regex, glob |

---

⭐ 如果这个项目对你有帮助，请给个 Star！
