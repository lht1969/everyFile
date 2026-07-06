# 使用虚拟滚动改进文件列表管理

## 背景

当前 `everything-tauri` 的文件列表（`ResultList.tsx`）将所有搜索结果一次性渲染为 DOM 节点，没有任何虚拟化处理。当搜索结果超过数千条时，DOM 节点数量线性增长，导致严重的性能问题——滚动卡顿、内存占用高、响应缓慢。

参考项目 `big_view` 实现了手写虚拟滚动（spacer + translateY 模式），在 100 万条数据下 DOM 节点数恒定在约 40 个，滚动流畅无卡顿。

## 当前问题清单

| 问题 | 严重性 | 位置 |
|------|--------|------|
| 无虚拟化，所有行渲染为 DOM 节点 | 严重 | `ResultList.tsx:214` |
| 后端线性扫描 O(n) 搜索 | 严重 | `monitor.rs:446` |
| 每次搜索重复调用 `to_lowercase()` | 高 | `monitor.rs:449-451` |
| 后端先全量构建结果再分页切片 | 高 | `commands/search.rs:73-84` |
| 前端重复排序（后端已排，前端 useMemo 再排） | 中 | `ResultList.tsx:60-76` |

## 三个阶段概览

- **第一阶段（最高优先级）**：前端虚拟滚动——移植 big_view 的 spacer + translateY 模式到 React
- **第二阶段（后续增强）**：后端游标分页——缓存搜索结果，按需切片返回
- **第三阶段（性能优化）**：后端搜索优化——预计算小写字段、减少克隆

---

## 第一阶段：前端虚拟滚动

### 核心思路

移植 big_view 的虚拟滚动模式到 React 组件：

```
视口容器（overflow-y: auto，监听滚动事件）
  └─ 占位垫片（spacer，高度 = 总条数 × 行高，用于创建正确比例的滚动条）
  └─ 内容容器（content，通过 translateY 定位到正确位置）
       └─ 只渲染可视范围内的行（约 35-50 个 DOM 节点）
```

关键参数：
- 行高（ROW_HEIGHT）：28 像素（从当前 24 像素稍微调大）
- 预渲染缓冲（overscan）：可视区域外多渲染 5 行
- 滚动节流：使用 `requestAnimationFrame` 确保帧级更新

### 任务 1.1：创建虚拟滚动钩子

**新建文件**：`src/hooks/useVirtualScroll.ts`

功能：封装虚拟滚动的核心计算逻辑，供 ResultList 组件使用。

输入参数：
- `totalItems`：总条目数
- `itemHeight`：每行高度（像素）
- `overscan`：预渲染缓冲行数，默认 5
- `containerRef`：滚动容器的 React 引用

输出结果：
- `startIndex`：可视范围起始索引
- `endIndex`：可视范围结束索引
- `offsetY`：内容容器的纵向偏移量（用于 translateY）
- `spacerHeight`：占位垫片的总高度（用于滚动条）
- `visibleItems`：可视范围内的条目数量

实现要点：
1. 监听容器的 scroll 事件
2. 用 `requestAnimationFrame` 包裹滚动回调
3. 根据 `scrollTop` 计算 `startIndex = Math.floor(scrollTop / 行高)`
4. 计算 `visibleItems = Math.ceil(可视区域高度 / 行高) + overscan`
5. 计算 `endIndex = Math.min(startIndex + visibleItems, totalItems)`
6. 返回偏移量和垫片高度供组件使用

### 任务 1.2：改造结果列表组件

**修改文件**：`src/components/ResultList.tsx`

当前渲染结构：
```tsx
<div className="result-body" ref={结果区域引用}>
  {所有结果.map((结果, 索引) => (
    <div className="result-row">列内容</div>
  ))}
</div>
```

改造后的结构：
```tsx
<div className="result-body" ref={结果区域引用}>
  <div className="virtual-spacer" style={{ height: 垫片高度 }} />
  <div className="virtual-content" style={{ transform: 'translateY(' + 偏移量 + 'px)' }}>
    {结果.slice(起始索引, 结束索引).map((结果, 本地索引) => (
      <div className="result-row" style={{ height: 行高 + 'px' }}>
        列内容（保持不变）
      </div>
    ))}
  </div>
</div>
```

具体改动：
1. 导入 `useVirtualScroll` 钩子
2. `ROW_HEIGHT` 从 24 改为 28
3. 添加占位垫片和内容容器的 DOM 结构
4. 调整 `scrollToIndex` 函数：直接设置 `scrollTop = 索引 × 行高`
5. 保持所有现有功能：排序 UI、右键菜单、悬浮提示、文件图标

### 任务 1.3：调整键盘导航

**修改文件**：`src/components/ResultList.tsx`

键盘导航已使用 `ROW_HEIGHT` 常量计算，需要适配虚拟滚动：

- 上下箭头：`selectedIndex` 加减 1，自动滚动到可视区域
- Page Up/Down：步长 = `Math.floor(可视区域高度 / ROW_HEIGHT)`
- Home/End：跳转到首/末条
- Enter：打开当前选中项
- 所有导航操作确保选中项在可视范围内

### 任务 1.4：CSS 样式调整

**修改文件**：`src/App.css`

添加虚拟滚动专用样式：

占位垫片样式：
- 绝对定位，宽 100%，高度由 JavaScript 动态设置
- 禁用鼠标事件（`pointer-events: none`）

内容容器样式：
- 相对定位，通过 `transform: translateY()` 定位

结果区域样式调整：
- 保持 `overflow-y: auto`
- 添加 `position: relative` 为子元素定位提供参考

### 任务 1.5：修复应用层数据流

**修改文件**：`src/App.tsx`

当前 `loadAllFiles` 只加载 1000 条结果，虚拟滚动需要感知总数量：

1. 添加 `totalResults` 状态变量，存储 `response.total`
2. 传递 `totalResults` 给 `ResultList` 组件
3. 搜索时同步更新 `totalResults`
4. 确保状态栏显示正确的总数

### 任务 1.6：移除冗余前端排序

**修改文件**：`src/components/ResultList.tsx`

后端 `monitor.rs` 的 `sort_results` 方法已按 `sort_by` 和 `sort_direction` 排序，前端 `useMemo` 再次排序是冗余操作。

改动：
1. 移除 `sortedResults` 的 `useMemo` 计算
2. 直接使用 `results` prop 进行渲染
3. 保留排序 UI 状态（列头高亮、排序箭头）
4. 后续如需客户端排序，改为通知后端重新排序

---

## 第二阶段：后端游标分页（后续增强）

### 任务 2.1：搜索结果缓存

**修改文件**：`src-tauri/src/index/monitor.rs`

新增 `SearchCache` 结构体：
- 存储上次搜索的查询条件（关键词、排序方式、过滤条件）
- 存储完整的搜索结果列表
- 记录创建时间，30 秒后过期
- 过期后自动失效，下次滚动触发重新搜索

在 `VolumeManager` 中添加缓存字段。

### 任务 2.2：新增按范围获取记录的命令

**修改文件**：`src-tauri/src/commands/search.rs`

新增 IPC 命令 `get_records_range`：
- 输入：起始索引、结束索引
- 从搜索缓存中按索引切片返回（O(1) 操作）
- 如果缓存不存在或已过期，返回错误让前端重新搜索

### 任务 2.3：前端按需获取

**修改文件**：`src/components/ResultList.tsx`

当用户滚动到新区域时：
1. 通过回调通知 App.tsx 当前可视范围
2. App.tsx 调用 `get_records_range` 获取对应数据切片
3. 更新 `results` 状态，触发重新渲染

---

## 第三阶段：后端性能优化

### 任务 3.1：预计算小写字段

**修改文件**：`src-tauri/src/search/mod.rs`

在 `SearchResult` 结构体中添加：
- `name_lower: String`（文件名的小写形式）
- `path_lower: String`（路径的小写形式）

扫描文件时一次性计算，避免每次搜索重复调用 `to_lowercase()`。

**修改文件**：`src-tauri/src/index/monitor.rs`

`search_with_query` 方法使用预计算字段代替运行时转换。

### 任务 3.2：减少克隆操作

**修改文件**：`src-tauri/src/index/monitor.rs`

搜索过程中使用引用而非克隆，仅在最终返回结果时按需克隆，减少内存分配。

---

## 需要修改的关键文件

| 文件 | 阶段 | 操作 |
|------|------|------|
| `src/hooks/useVirtualScroll.ts` | 第一阶段 | 新建 |
| `src/components/ResultList.tsx` | 第一阶段 | 重写虚拟滚动部分 |
| `src/App.css` | 第一阶段 | 添加虚拟滚动样式 |
| `src/App.tsx` | 第一阶段 | 添加总数量状态 |
| `src-tauri/src/commands/search.rs` | 第二阶段 | 新增按范围获取命令 |
| `src-tauri/src/index/monitor.rs` | 第二、三阶段 | 缓存+优化 |
| `src-tauri/src/search/mod.rs` | 第三阶段 | 预计算字段 |

## 验证方案

### 第一阶段验证

1. 启动应用，执行空查询获取大量结果
2. 快速滚动列表，确认无卡顿、无白屏
3. 打开浏览器开发者工具的元素面板，确认 DOM 节点数恒定在 40-60 个
4. 测试键盘导航：上下箭头、PageUp/PageDown、Home/End、Enter
5. 测试右键菜单：打开、打开文件夹、复制路径、删除
6. 测试双击打开文件/文件夹
7. 测试悬浮提示（500ms 延迟）
8. 测试列头排序切换（名称、大小、修改时间）

### 第二阶段验证

1. 滚动到列表中间位置，确认数据正确显示
2. 快速来回滚动，确认缓存命中（通过日志观察）
3. 等待 30 秒后滚动，确认缓存过期后重新搜索

### 第三阶段验证

1. 搜索简单关键词（如 "test"），对比优化前后搜索耗时
2. 确认搜索结果一致性（排序、过滤条件）
3. 通过日志确认 `to_lowercase()` 调用次数减少
