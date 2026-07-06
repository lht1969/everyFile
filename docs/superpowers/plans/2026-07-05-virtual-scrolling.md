# Virtual Scrolling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add virtual scrolling to the file manager's result list so only visible rows are rendered as DOM nodes, enabling smooth scrolling through millions of results.

**Architecture:** Adapt the big_view spacer+translateY pattern into the existing React ResultList component. A spacer div sets scrollbar height to `totalResults * ROW_HEIGHT`; a content div uses `translateY(startIndex * ROW_HEIGHT)` to position only the visible slice. Backend cursor-based pagination (Phase 2) caches sorted results server-side so scroll fetches are O(1) Vec slicing.

**Tech Stack:** React 18 + TypeScript (frontend), Rust + Tauri IPC (backend), no new dependencies.

---

## Current State Summary

| Aspect | Current | After Phase 1 | After Phase 2 |
|--------|---------|---------------|---------------|
| DOM nodes | All results (up to 1000) | ~35 visible rows | ~35 visible rows |
| Scroll perf | Degrades with count | Constant | Constant |
| Data in memory | All results in React state | All results in React state | Cached server-side, sliced on demand |
| Backend | Linear scan + sort + paginate | Same | Search once, cache, O(1) slice |

## File Map

| File | Role | Phase |
|------|------|-------|
| `src/components/ResultList.tsx` | Main virtual scroll implementation | 1 |
| `src/components/VirtualList.tsx` | NEW: Reusable virtual scroll hook | 1 |
| `src/App.tsx` | Pass total count to ResultList | 1 |
| `src/App.css` | Minimal CSS tweaks for virtual scroll | 1 |
| `src-tauri/src/commands/search.rs` | New `get_records_range` command, search cache | 2 |
| `src-tauri/src/index/monitor.rs` | Cache sorted results, O(1) slice access | 2 |
| `src-tauri/src/search/mod.rs` | `SearchCache` struct | 2 |

---

## Phase 1: Frontend Virtual Scrolling

### Task 1: Create the `useVirtualScroll` hook

**Files:**
- Create: `src/hooks/useVirtualScroll.ts`

This is a pure computation hook — no DOM, no side effects beyond the ref.

- [ ] **Step 1: Create the hook file**

```typescript
// src/hooks/useVirtualScroll.ts
import { useState, useCallback, useRef, useEffect } from 'react';

export interface VirtualScrollOptions {
  totalItems: number;
  itemHeight: number;
  containerRef: React.RefObject<HTMLDivElement>;
  overscan?: number; // extra rows above/below viewport, default 5
}

export interface VirtualScrollState {
  startIndex: number;
  endIndex: number;
  offsetY: number;
  totalHeight: number;
  visibleItems: number[];
}

export function useVirtualScroll({
  totalItems,
  itemHeight,
  containerRef,
  overscan = 5,
}: VirtualScrollOptions): VirtualScrollState {
  const [scrollTop, setScrollTop] = useState(0);
  const rafId = useRef<number>(0);

  const handleScroll = useCallback(() => {
    if (rafId.current) cancelAnimationFrame(rafId.current);
    rafId.current = requestAnimationFrame(() => {
      if (containerRef.current) {
        setScrollTop(containerRef.current.scrollTop);
      }
    });
  }, [containerRef]);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    el.addEventListener('scroll', handleScroll, { passive: true });
    return () => {
      el.removeEventListener('scroll', handleScroll);
      if (rafId.current) cancelAnimationFrame(rafId.current);
    };
  }, [containerRef, handleScroll]);

  const viewportHeight = containerRef.current?.clientHeight ?? 0;
  const visibleCount = Math.ceil(viewportHeight / itemHeight);

  const rawStart = Math.floor(scrollTop / itemHeight);
  const startIndex = Math.max(0, rawStart - overscan);
  const endIndex = Math.min(totalItems, rawStart + visibleCount + overscan);

  const items: number[] = [];
  for (let i = startIndex; i < endIndex; i++) {
    items.push(i);
  }

  return {
    startIndex,
    endIndex,
    offsetY: startIndex * itemHeight,
    totalHeight: totalItems * itemHeight,
    visibleItems: items,
  };
}
```

- [ ] **Step 2: Verify it compiles**

Run: `npx tsc --noEmit`
Expected: No errors (hook is pure TypeScript, no new deps).

- [ ] **Step 3: Commit**

```bash
git add src/hooks/useVirtualScroll.ts
git commit -m "feat: add useVirtualScroll hook for virtual list computation"
```

---

### Task 2: Adapt ResultList.tsx for virtual scrolling

**Files:**
- Modify: `src/components/ResultList.tsx` (full rewrite of render logic)

Key changes:
1. Import `useVirtualScroll` from the new hook.
2. Replace the `sortedResults.map(...)` block with spacer + translateY pattern.
3. Keep all existing state/handlers (sort, keyboard nav, context menu, tooltip).
4. Add `totalResults` prop (will come from App.tsx after Phase 2; for now, use `results.length`).

- [ ] **Step 1: Rewrite the ResultList component**

Replace the entire content of `src/components/ResultList.tsx` with:

```typescript
import { useState, useMemo, useRef, useEffect, useCallback } from 'react';
import { useVirtualScroll } from '../hooks/useVirtualScroll';

interface SearchResult {
  file_id: number;
  name: string;
  path: string;
  size: number;
  modified_time: string;
  is_directory: boolean;
  formatted_size: string;
  formatted_modified_time: string;
}

interface ResultListProps {
  results: SearchResult[];
  totalResults?: number; // total across all pages (Phase 2); defaults to results.length
  onOpenFile: (path: string) => void;
  onOpenFolder: (path: string) => void;
  onDeleteFile?: (path: string) => void;
  onVisibleRangeChange?: (start: number, end: number) => void; // Phase 2 callback
}

type SortField = 'name' | 'size' | 'modified_time';
type SortDirection = 'asc' | 'desc';

const ROW_HEIGHT = 24;
const OVERSCAN = 5;

function ResultList({
  results,
  totalResults,
  onOpenFile,
  onOpenFolder,
  onDeleteFile,
  onVisibleRangeChange,
}: ResultListProps) {
  const [sortField, setSortField] = useState<SortField>('name');
  const [sortDirection, setSortDirection] = useState<SortDirection>('asc');
  const [selectedIndex, setSelectedIndex] = useState(-1);
  const [contextMenu, setContextMenu] = useState<{
    x: number; y: number; path: string; isDirectory: boolean;
  } | null>(null);
  const [hoveredItem, setHoveredItem] = useState<{
    index: number; x: number; y: number; data: SearchResult;
  } | null>(null);
  const [showTooltip, setShowTooltip] = useState(false);
  const resultBodyRef = useRef<HTMLDivElement>(null);
  const hoverTimeoutRef = useRef<number | null>(null);

  const effectiveTotal = totalResults ?? results.length;

  const { startIndex, endIndex, offsetY, totalHeight, visibleItems } =
    useVirtualScroll({
      totalItems: effectiveTotal,
      itemHeight: ROW_HEIGHT,
      containerRef: resultBodyRef,
      overscan: OVERSCAN,
    });

  // Notify parent when visible range changes (for Phase 2 on-demand fetch)
  const prevRangeRef = useRef({ start: -1, end: -1 });
  useEffect(() => {
    if (
      onVisibleRangeChange &&
      (startIndex !== prevRangeRef.current.start ||
        endIndex !== prevRangeRef.current.end)
    ) {
      prevRangeRef.current = { start: startIndex, end: endIndex };
      onVisibleRangeChange(startIndex, endIndex);
    }
  }, [startIndex, endIndex, onVisibleRangeChange]);

  // --- Sort (unchanged logic) ---
  const sortedResults = useMemo(() => {
    return [...results].sort((a, b) => {
      let comparison = 0;
      switch (sortField) {
        case 'name':
          comparison = a.name.localeCompare(b.name);
          break;
        case 'size':
          comparison = a.size - b.size;
          break;
        case 'modified_time':
          comparison = a.modified_time.localeCompare(b.modified_time);
          break;
      }
      return sortDirection === 'asc' ? comparison : -comparison;
    });
  }, [results, sortField, sortDirection]);

  const handleSort = (field: SortField) => {
    if (field === sortField) {
      setSortDirection(sortDirection === 'asc' ? 'desc' : 'asc');
    } else {
      setSortField(field);
      setSortDirection('asc');
    }
  };

  // --- Helpers ---
  const getDirectoryPath = (path: string, isDirectory: boolean): string => {
    if (isDirectory) {
      return path.endsWith('\\') ? path : path + '\\';
    }
    const lastBackslash = path.lastIndexOf('\\');
    if (lastBackslash > 0) {
      return path.substring(0, lastBackslash);
    }
    return path;
  };

  useEffect(() => {
    return () => {
      if (hoverTimeoutRef.current) clearTimeout(hoverTimeoutRef.current);
    };
  }, []);

  // --- Row interaction ---
  const handleRowClick = (index: number) => setSelectedIndex(index);

  const handleRowDoubleClick = (path: string, isDirectory: boolean) => {
    if (isDirectory) onOpenFolder(path);
    else onOpenFile(path);
  };

  // --- Keyboard navigation ---
  const scrollToIndex = (index: number) => {
    if (resultBodyRef.current) {
      const container = resultBodyRef.current;
      const scrollTop = container.scrollTop;
      const containerHeight = container.clientHeight;
      const itemTop = index * ROW_HEIGHT;
      const itemBottom = itemTop + ROW_HEIGHT;

      if (itemTop < scrollTop) {
        container.scrollTop = itemTop;
      } else if (itemBottom > scrollTop + containerHeight) {
        container.scrollTop = itemBottom - containerHeight;
      }
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setSelectedIndex((prev) => {
        const next = Math.min(prev + 1, effectiveTotal - 1);
        scrollToIndex(next);
        return next;
      });
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setSelectedIndex((prev) => {
        const next = Math.max(prev - 1, 0);
        scrollToIndex(next);
        return next;
      });
    } else if (e.key === 'Home') {
      e.preventDefault();
      setSelectedIndex(0);
      if (resultBodyRef.current) resultBodyRef.current.scrollTop = 0;
    } else if (e.key === 'End') {
      e.preventDefault();
      const last = effectiveTotal - 1;
      setSelectedIndex(last);
      if (resultBodyRef.current)
        resultBodyRef.current.scrollTop = resultBodyRef.current.scrollHeight;
    } else if (e.key === 'PageDown') {
      e.preventDefault();
      if (resultBodyRef.current) {
        const next = Math.min(
          selectedIndex +
            Math.floor(resultBodyRef.current.clientHeight / ROW_HEIGHT),
          effectiveTotal - 1
        );
        setSelectedIndex(next);
        scrollToIndex(next);
      }
    } else if (e.key === 'PageUp') {
      e.preventDefault();
      if (resultBodyRef.current) {
        const next = Math.max(
          selectedIndex -
            Math.floor(resultBodyRef.current.clientHeight / ROW_HEIGHT),
          0
        );
        setSelectedIndex(next);
        scrollToIndex(next);
      }
    } else if (e.key === 'Enter' && selectedIndex >= 0 && selectedIndex < sortedResults.length) {
      const item = sortedResults[selectedIndex];
      handleRowDoubleClick(item.path, item.is_directory);
    }
  };

  const getSortIcon = (field: SortField) => {
    if (sortField !== field) return '';
    return sortDirection === 'asc' ? ' ▲' : ' ▼';
  };

  // --- Context menu ---
  const handleContextMenu = (
    e: React.MouseEvent, path: string, isDirectory: boolean
  ) => {
    e.preventDefault();
    setShowTooltip(false);
    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
      hoverTimeoutRef.current = null;
    }
    setContextMenu({ x: e.clientX, y: e.clientY, path, isDirectory });
  };

  const closeContextMenu = () => setContextMenu(null);

  // --- Tooltip ---
  const handleMouseEnter = (e: React.MouseEvent, index: number, data: SearchResult) => {
    const rect = (e.target as HTMLElement).getBoundingClientRect();
    setHoveredItem({ index, x: rect.left, y: rect.bottom, data });
    if (hoverTimeoutRef.current) clearTimeout(hoverTimeoutRef.current);
    hoverTimeoutRef.current = setTimeout(() => setShowTooltip(true), 500);
  };

  const handleMouseLeave = () => {
    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
      hoverTimeoutRef.current = null;
    }
    setHoveredItem(null);
    setShowTooltip(false);
  };

  // --- Render a single row (extracted for clarity) ---
  const renderRow = (virtualIndex: number) => {
    // virtualIndex maps to sortedResults index while we have all data client-side.
    // Phase 2 will map virtualIndex -> cached server-side index.
    if (virtualIndex >= sortedResults.length) return null;
    const result = sortedResults[virtualIndex];
    return (
      <div
        key={`${virtualIndex}-${result.path}`}
        className={`result-row ${virtualIndex === selectedIndex ? 'selected' : ''}`}
        onClick={() => handleRowClick(virtualIndex)}
        onDoubleClick={() =>
          handleRowDoubleClick(result.path, result.is_directory)
        }
        onContextMenu={(e) =>
          handleContextMenu(e, result.path, result.is_directory)
        }
        onMouseEnter={(e) => handleMouseEnter(e, virtualIndex, result)}
        onMouseLeave={handleMouseLeave}
      >
        <div className="col-name">
          <span className="file-icon">
            {result.is_directory ? '📁' : '📄'}
          </span>
          {result.name}
        </div>
        <div className="col-path" title={result.path}>
          {getDirectoryPath(result.path, result.is_directory)}
        </div>
        <div className="col-size">{result.formatted_size}</div>
        <div className="col-modified">{result.formatted_modified_time}</div>
      </div>
    );
  };

  return (
    <div className="result-list" tabIndex={0} onKeyDown={handleKeyDown} onClick={closeContextMenu}>
      <div className="result-table">
        <div className="result-row header">
          <div className="col-name" onClick={() => handleSort('name')}>
            名称{getSortIcon('name')}
          </div>
          <div className="col-path">路径</div>
          <div className="col-size" onClick={() => handleSort('size')}>
            大小{getSortIcon('size')}
          </div>
          <div className="col-modified" onClick={() => handleSort('modified_time')}>
            修改时间{getSortIcon('modified_time')}
          </div>
        </div>
        <div className="result-body" ref={resultBodyRef}>
          {/* Spacer sets the scrollbar height */}
          <div style={{ height: totalHeight, position: 'relative' }}>
            {/* Content div positions visible rows */}
            <div
              style={{
                transform: `translateY(${offsetY}px)`,
                position: 'absolute',
                width: '100%',
              }}
            >
              {visibleItems.map((i) => renderRow(i))}
            </div>
          </div>
        </div>
      </div>
      {contextMenu && (
        <div
          className="context-menu"
          style={{ left: contextMenu.x, top: contextMenu.y }}
          onClick={(e) => e.stopPropagation()}
        >
          <div
            className="context-menu-item"
            onClick={() => {
              onOpenFile(contextMenu.path);
              closeContextMenu();
            }}
          >
            打开
          </div>
          <div
            className="context-menu-item"
            onClick={() => {
              onOpenFolder(contextMenu.path);
              closeContextMenu();
            }}
          >
            打开文件夹
          </div>
          <div
            className="context-menu-item"
            onClick={() => {
              navigator.clipboard.writeText(contextMenu.path);
              closeContextMenu();
            }}
          >
            复制路径
          </div>
          {onDeleteFile && (
            <div
              className="context-menu-item danger"
              onClick={() => {
                onDeleteFile(contextMenu.path);
                closeContextMenu();
              }}
            >
              删除
            </div>
          )}
        </div>
      )}
      {hoveredItem && showTooltip && (
        <div
          className="hover-tooltip"
          style={{ left: hoveredItem.x, top: hoveredItem.y }}
        >
          <div className="hover-tooltip-row">
            <strong>名称:</strong> {hoveredItem.data.name}
          </div>
          <div className="hover-tooltip-row">
            <strong>大小:</strong> {hoveredItem.data.formatted_size}
          </div>
          <div className="hover-tooltip-row">
            <strong>日期:</strong> {hoveredItem.data.formatted_modified_time}
          </div>
          <div className="hover-tooltip-row">
            <strong>路径:</strong> {hoveredItem.data.path}
          </div>
        </div>
      )}
    </div>
  );
}

export default ResultList;
```

- [ ] **Step 2: Verify TypeScript compiles**

Run: `npx tsc --noEmit`
Expected: No errors.

- [ ] **Step 3: Verify the app builds**

Run: `npm run build`
Expected: Build succeeds with no errors.

- [ ] **Step 4: Manual smoke test**

Run: `npm run dev` then open in browser.
Expected:
- Empty search shows header only, no empty scroll space.
- Short lists (< 50 items) render all rows, scrollbar absent or tiny.
- Long lists show a tall scrollbar; scrolling is smooth at 60fps.
- Selected row stays correct when scrolling.
- Keyboard Up/Down/Home/End/PageUp/PageDown all work.
- Context menu and tooltip still appear at correct positions.

- [ ] **Step 5: Commit**

```bash
git add src/components/ResultList.tsx src/hooks/useVirtualScroll.ts
git commit -m "feat: add virtual scrolling to ResultList - only visible rows rendered"
```

---

### Task 3: Fix CSS for virtual scroll container

**Files:**
- Modify: `src/App.css` (target `.result-body`)

The `.result-body` already has `overflow-y: auto` which is correct. We need to ensure the inner spacer div works properly.

- [ ] **Step 1: Add virtual-scroll specific styles**

Add these rules after the existing `.result-body` rule (line 324):

```css
/* Virtual scroll: spacer div creates scrollbar, content div positions rows */
.result-body > div {
  position: relative;
}
```

- [ ] **Step 2: Verify no visual regressions**

Run: `npm run dev` and check:
- Header row stays sticky at top (it's outside `.result-body`).
- Rows align with header columns.
- Scrollbar appears on the right edge of `.result-body`.
- No double scrollbars.

- [ ] **Step 3: Commit**

```bash
git add src/App.css
git commit -m "fix: CSS adjustments for virtual scroll container"
```

---

### Task 4: Wire `totalResults` from App.tsx

**Files:**
- Modify: `src/App.tsx` (pass total to ResultList)

Currently `App.tsx` passes `results` (array) to ResultList. We need to also pass `total` so the virtual scroll knows the full count even when only a subset is loaded.

For Phase 1 (all data client-side), `total = results.length`. This prepares the interface for Phase 2 where total diverges from results.length.

- [ ] **Step 1: Add totalResults to the ResultList call**

In `src/App.tsx`, find the `<ResultList` component (around line 240) and add the `totalResults` prop:

```tsx
<ResultList
  results={results}
  totalResults={pagination.total}
  onOpenFile={handleOpenFile}
  onOpenFolder={handleOpenFolder}
  onDeleteFile={handleDeleteFile}
/>
```

- [ ] **Step 2: Verify TypeScript compiles**

Run: `npx tsc --noEmit`
Expected: No errors.

- [ ] **Step 3: Commit**

```bash
git add src/App.tsx
git commit -m "feat: pass totalResults from App to ResultList for virtual scroll"
```

---

### Task 5: Handle edge case — selected index beyond loaded results

**Files:**
- Modify: `src/components/ResultList.tsx`

When Phase 2 is active, `sortedResults.length` may be less than `effectiveTotal`. The selected index could point beyond the loaded data. We need a guard.

- [ ] **Step 1: Add guard in renderRow**

In the `renderRow` function, the guard `if (virtualIndex >= sortedResults.length) return null;` already handles this. But we also need to clamp `selectedIndex` when results shrink.

Add this `useEffect` after the sort useMemo:

```typescript
// Clamp selectedIndex when results shrink (e.g., new search)
useEffect(() => {
  if (selectedIndex >= sortedResults.length && sortedResults.length > 0) {
    setSelectedIndex(sortedResults.length - 1);
  }
}, [sortedResults.length, selectedIndex]);
```

- [ ] **Step 2: Verify TypeScript compiles**

Run: `npx tsc --noEmit`

- [ ] **Step 3: Commit**

```bash
git add src/components/ResultList.tsx
git commit -m "fix: clamp selectedIndex when result list shrinks"
```

---

### Task 6: Performance verification — large dataset test

**Files:** None (manual testing)

This task verifies that virtual scrolling actually improves performance with large result sets.

- [ ] **Step 1: Create a test script to populate large data**

In `src-tauri/src/commands/search.rs`, temporarily modify `loadAllFiles` in App.tsx to request `page_size: 100000` (or use the backend's existing `max_results: 5000000`).

Alternative: Create a mock dataset by modifying `VolumeMonitor::search_with_options` to duplicate results for testing purposes. Add a `#[cfg(debug_assertions)]` block:

```rust
// Temporary test code — remove before merge
#[cfg(debug_assertions)]
{
    let original_len = results.len();
    for i in 1..100 {
        let mut batch: Vec<SearchResult> = results.iter().take(original_len).cloned().collect();
        for (j, item) in batch.iter_mut().enumerate() {
            item.file_id = (i * original_len + j) as u64;
            item.name = format!("{} (copy {})", item.name, i);
        }
        results.extend(batch);
        if results.len() >= 100_000 {
            results.truncate(100_000);
            break;
        }
    }
    total_count = results.len();
}
```

- [ ] **Step 2: Measure before/after**

Before virtual scrolling (revert temporarily):
- Open Chrome DevTools → Performance tab
- Scroll through 100K rows
- Record: frame rate, layout/paint time, JS heap

After virtual scrolling:
- Same measurement
- Expected: consistent 60fps, ~35 DOM nodes regardless of total count

- [ ] **Step 3: Remove test code and commit**

```bash
git add -A
git commit -m "test: verify virtual scroll performance with 100K rows"
```

---

## Phase 2: Backend Cursor-Based Pagination

### Task 7: Add `SearchCache` to VolumeManager

**Files:**
- Modify: `src-tauri/src/index/monitor.rs`
- Modify: `src-tauri/src/search/mod.rs`

The cache stores the last search's sorted results so subsequent scroll fetches are O(1) Vec slicing instead of re-running the search.

- [ ] **Step 1: Define SearchCache struct in `src-tauri/src/search/mod.rs`**

Add after the `SearchOptions` struct:

```rust
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Cache key derived from search parameters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchCacheKey {
    pub query: String,
    pub files_only: bool,
    pub directories_only: bool,
    pub sort_by: SortBy,
    pub sort_direction: SortDirection,
}

/// Cached search results for O(1) range access during scrolling.
#[derive(Debug, Clone)]
pub struct SearchCache {
    pub key: SearchCacheKey,
    pub results: Vec<SearchResult>,
    pub total: usize,
    pub created_at: std::time::Instant,
}

impl SearchCache {
    /// Returns true if cache is still valid (within 30 seconds).
    pub fn is_valid(&self) -> bool {
        self.created_at.elapsed() < std::time::Duration::from_secs(30)
    }

    /// Slice the cached results for a given range.
    pub fn get_range(&self, start: usize, end: usize) -> &[SearchResult] {
        let s = start.min(self.results.len());
        let e = end.min(self.results.len());
        &self.results[s..e]
    }
}
```

- [ ] **Step 2: Add cache field to VolumeManager in `src-tauri/src/index/monitor.rs`**

Add a `search_cache` field:

```rust
use crate::search::{SearchCache, SearchCacheKey};

pub struct VolumeManager {
    volumes: HashMap<String, VolumeMonitor>,
    search_cache: Option<SearchCache>,
}
```

Update `VolumeManager::new()`:

```rust
pub fn new() -> Self {
    Self {
        volumes: HashMap::new(),
        search_cache: None,
    }
}
```

- [ ] **Step 3: Update `search_with_options` to populate cache**

At the end of `search_with_options`, before returning, store the results:

```rust
// After sorting, before return
let cache_key = SearchCacheKey {
    query: query.to_string(),
    files_only: options.files_only,
    directories_only: options.directories_only,
    sort_by: options.sort_by,
    sort_direction: options.sort_direction,
};

self.search_cache = Some(SearchCache {
    key: cache_key,
    results: results.clone(),
    total: total_count,
    created_at: std::time::Instant::now(),
});

(results, total_count)
```

- [ ] **Step 4: Add `get_cached_range` method to VolumeManager**

```rust
/// Returns a slice of cached search results. Returns None if cache is stale or key mismatch.
pub fn get_cached_range(
    &self,
    key: &SearchCacheKey,
    start: usize,
    end: usize,
) -> Option<(&[SearchResult], usize)> {
    if let Some(ref cache) = self.search_cache {
        if cache.key == *key && cache.is_valid() {
            let total = cache.total;
            return Some((cache.get_range(start, end), total));
        }
    }
    None
}
```

- [ ] **Step 5: Verify Rust compiles**

Run: `cargo check` in `src-tauri/`
Expected: No errors.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/search/mod.rs src-tauri/src/index/monitor.rs
git commit -m "feat: add SearchCache to VolumeManager for O(1) range access"
```

---

### Task 8: Add `get_records_range` IPC command

**Files:**
- Modify: `src-tauri/src/commands/search.rs`
- Modify: `src-tauri/src/main.rs` (register new command)

- [ ] **Step 1: Define the new command in `src-tauri/src/commands/search.rs`**

Add after the `search_files` command:

```rust
use crate::search::SearchCacheKey;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecordsRangeParams {
    pub query: String,
    pub files_only: bool,
    pub directories_only: bool,
    pub sort_by: Option<String>,
    pub sort_direction: Option<String>,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecordsRangeResponse {
    pub results: Vec<SearchResult>,
    pub total: usize,
    pub start: usize,
    pub end: usize,
}

#[tauri::command]
pub async fn get_records_range(
    state: State<'_, AppState>,
    params: GetRecordsRangeParams,
) -> Result<GetRecordsRangeResponse, String> {
    let sort_by = match params.sort_by.as_deref() {
        Some("name") => crate::search::SortBy::Name,
        Some("size") => crate::search::SortBy::Size,
        Some("modified") => crate::search::SortBy::ModifiedTime,
        _ => crate::search::SortBy::Score,
    };
    let sort_direction = match params.sort_direction.as_deref() {
        Some("asc") => crate::search::SortDirection::Ascending,
        _ => crate::search::SortDirection::Descending,
    };

    let key = SearchCacheKey {
        query: params.query.clone(),
        files_only: params.files_only,
        directories_only: params.directories_only,
        sort_by,
        sort_direction,
    };

    let vm = state.volume_manager.lock().await;

    // Try cache first
    if let Some((slice, total)) = vm.get_cached_range(&key, params.start, params.end) {
        return Ok(GetRecordsRangeResponse {
            results: slice.to_vec(),
            total,
            start: params.start,
            end: params.end,
        });
    }

    // Cache miss: run full search, cache it, then return the slice
    let mut options = crate::search::SearchOptions::default();
    options.sort_by = sort_by;
    options.sort_direction = sort_direction;
    options.files_only = params.files_only;
    options.directories_only = params.directories_only;

    let (all_results, total) = vm.search_with_options(&params.query, &options);
    let slice: Vec<SearchResult> = all_results
        .into_iter()
        .skip(params.start)
        .take(params.end - params.start)
        .collect();

    Ok(GetRecordsRangeResponse {
        results: slice,
        total,
        start: params.start,
        end: params.end,
    })
}
```

- [ ] **Step 2: Register the command in `src-tauri/src/main.rs`**

Add `commands::search::get_records_range` to the `invoke_handler` list:

```rust
.invoke_handler(tauri::generate_handler![
    commands::search::search_files,
    commands::search::get_search_suggestions,
    commands::search::get_records_range,  // <-- add this line
    // ... rest unchanged
])
```

- [ ] **Step 3: Verify Rust compiles**

Run: `cargo check` in `src-tauri/`
Expected: No errors.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/commands/search.rs src-tauri/src/main.rs
git commit -m "feat: add get_records_range IPC command with cache-backed O(1) slicing"
```

---

### Task 9: Frontend on-demand fetch integration

**Files:**
- Modify: `src/App.tsx`
- Modify: `src/components/ResultList.tsx`

Wire the `onVisibleRangeChange` callback to fetch only visible rows from the backend.

- [ ] **Step 1: Add onVisibleRangeChange handler in App.tsx**

```typescript
import { useCallback, useRef } from 'react';

// Add inside App component, after handleSearch:
const lastFetchRef = useRef({ start: -1, end: -1 });

const handleVisibleRangeChange = useCallback(async (start: number, end: number) => {
  // Avoid refetching the same range
  if (start === lastFetchRef.current.start && end === lastFetchRef.current.end) return;
  lastFetchRef.current = { start, end };

  try {
    const response = await invoke<GetRecordsRangeResponse>('get_records_range', {
      params: {
        query: searchState.query,
        files_only: searchState.filesOnly,
        directories_only: searchState.directoriesOnly,
        sort_by: sortField,    // need to lift sort state to App
        sort_direction: sortDirection,
        start,
        end,
      },
    });
    // Replace results with the fetched slice
    // Build a full-sized sparse array or use a different data model
    setResults((prev) => {
      const updated = [...prev];
      response.results.forEach((r, i) => {
        updated[start + i] = r;
      });
      return updated;
    });
  } catch (e) {
    console.error('Failed to fetch range:', e);
  }
}, [searchState]);
```

Add the interface:

```typescript
interface GetRecordsRangeResponse {
  results: SearchResult[];
  total: number;
  start: number;
  end: number;
}
```

Pass to ResultList:

```tsx
<ResultList
  results={results}
  totalResults={pagination.total}
  onOpenFile={handleOpenFile}
  onOpenFolder={handleOpenFolder}
  onDeleteFile={handleDeleteFile}
  onVisibleRangeChange={handleVisibleRangeChange}
/>
```

- [ ] **Step 2: Lift sort state from ResultList to App**

Currently sorting is local to ResultList. For Phase 2, the backend needs to know the sort params to return correctly sorted slices.

Option A (simpler): Keep frontend sorting for now; the backend always returns by name ascending, and the frontend re-sorts. This avoids lifting sort state.

Option B (recommended for correctness): Move `sortField`/`sortDirection` state to App.tsx and pass them as props to ResultList. The `handleVisibleRangeChange` then includes sort params.

For Option B, add to App.tsx:

```typescript
const [sortField, setSortField] = useState<'name' | 'size' | 'modified_time'>('name');
const [sortDirection, setSortDirection] = useState<'asc' | 'desc'>('asc');
```

Pass to ResultList:

```tsx
<ResultList
  results={results}
  totalResults={pagination.total}
  sortField={sortField}
  sortDirection={sortDirection}
  onSortChange={(field, dir) => { setSortField(field); setSortDirection(dir); }}
  onOpenFile={handleOpenFile}
  onOpenFolder={handleOpenFolder}
  onDeleteFile={handleDeleteFile}
  onVisibleRangeChange={handleVisibleRangeChange}
/>
```

In ResultList, change sort state from `useState` to controlled props:

```typescript
interface ResultListProps {
  // ... existing
  sortField?: SortField;
  sortDirection?: SortDirection;
  onSortChange?: (field: SortField, direction: SortDirection) => void;
}

// Replace useState for sort with:
const effectiveSortField = props.sortField ?? sortFieldLocal;
const effectiveSortDirection = props.sortDirection ?? sortDirectionLocal;
```

- [ ] **Step 3: Verify TypeScript compiles**

Run: `npx tsc --noEmit`

- [ ] **Step 4: Commit**

```bash
git add src/App.tsx src/components/ResultList.tsx
git commit -m "feat: integrate on-demand fetch via get_records_range on scroll"
```

---

### Task 10: Remove redundant frontend sort (Phase 2 optimization)

**Files:**
- Modify: `src/components/ResultList.tsx`

Once the backend returns sorted results, the frontend `useMemo` sort is redundant. Remove it and use `results` directly.

- [ ] **Step 1: Remove sortedResults useMemo**

Replace:

```typescript
const sortedResults = useMemo(() => {
  return [...results].sort((a, b) => {
    // ...
  });
}, [results, sortField, sortDirection]);
```

With:

```typescript
// Backend returns sorted results; use directly
const sortedResults = results;
```

- [ ] **Step 2: Verify no regressions**

Run: `npm run dev` and verify sorting still works (now driven by backend).

- [ ] **Step 3: Commit**

```bash
git add src/components/ResultList.tsx
git commit -m "refactor: remove redundant frontend sort, backend handles ordering"
```

---

## Phase 3: Backend Performance Optimizations

### Task 11: Cache `to_lowercase` during search

**Files:**
- Modify: `src-tauri/src/index/monitor.rs`

The `search_with_query` method calls `to_lowercase()` on every file name and path for every search. Cache these.

- [ ] **Step 1: Add lowercase cache to VolumeMonitor**

```rust
pub struct VolumeMonitor {
    drive_letter: String,
    files: Vec<SearchResult>,
    include_hidden_files: bool,
    include_system_files: bool,
    // Pre-computed lowercase names/paths for fast search
    names_lower: Vec<String>,
    paths_lower: Vec<String>,
}
```

In `scan()`, after pushing each file, also push lowercase versions:

```rust
self.names_lower.push(result.name.to_lowercase());
self.paths_lower.push(result.path.to_lowercase());
```

- [ ] **Step 2: Update `search_with_query` to use cached lowercase**

```rust
fn search_with_query(&self, query: &crate::search::query::SearchQuery) -> Vec<SearchResult> {
    let keywords_lower: Vec<String> = query.keywords.iter()
        .map(|kw| kw.to_lowercase())
        .collect();

    let path_filter_lower = query.path_filter.as_ref().map(|pf| pf.to_lowercase());

    let mut results = Vec::new();

    for (i, f) in self.files.iter().enumerate() {
        let name_lower = &self.names_lower[i];
        let path_lower = &self.paths_lower[i];

        if !keywords_lower.is_empty() {
            let all_keywords_match = keywords_lower.iter().all(|kw| name_lower.contains(kw.as_str()));
            if !all_keywords_match {
                continue;
            }
        }

        // ... rest of filtering using path_lower instead of f.path.to_lowercase()

        results.push(f.clone());
    }

    results
}
```

- [ ] **Step 3: Update `search` (the simple one) similarly**

```rust
fn search(&self, query: &str) -> Vec<SearchResult> {
    let query_lower = query.to_lowercase();
    self.files
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            self.names_lower[*i].contains(&query_lower)
                || self.paths_lower[*i].contains(&query_lower)
        })
        .map(|(_, f)| f.clone())
        .collect()
}
```

- [ ] **Step 4: Verify Rust compiles**

Run: `cargo check`
Expected: No errors.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/index/monitor.rs
git commit -m "perf: cache to_lowercase results in VolumeMonitor for faster search"
```

---

### Task 12: Reduce clone operations

**Files:**
- Modify: `src-tauri/src/index/monitor.rs`

Several places clone entire `Vec<SearchResult>` unnecessarily.

- [ ] **Step 1: Change `get_all_files` to return references where possible**

The `get_all_files()` method clones all files. If callers can work with references, change the signature. For now, only `search_with_options` calls it for empty queries — we can optimize that path.

In `search_with_options`, for the empty-query path, avoid cloning all files:

```rust
// Instead of:
let volume_results = monitor.get_all_files();
// Use:
let volume_results: Vec<&SearchResult> = monitor.files.iter().collect();
```

Adjust the downstream iteration to use references instead of owned values.

- [ ] **Step 2: Verify Rust compiles and search works**

Run: `cargo check` then `cargo test` (if tests exist).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/index/monitor.rs
git commit -m "perf: reduce unnecessary clone operations in search path"
```

---

## Verification Checklist

After all phases are complete, verify:

- [ ] Empty query shows all files, virtual scrolling works
- [ ] Search with results < 50 renders all rows, no visual artifacts
- [ ] Search with results > 1000 shows virtual scrollbar, smooth 60fps scroll
- [ ] Keyboard navigation (arrows, Home/End, PageUp/PageDown) works correctly
- [ ] Selected row stays visible when scrolling
- [ ] Context menu appears at correct position for visible rows
- [ ] Tooltip appears at correct position for visible rows
- [ ] Sorting works (name, size, modified_time, asc/desc)
- [ ] Sort column header shows ▲/▼ indicator
- [ ] Double-click opens file/folder correctly
- [ ] Export works with large result sets
- [ ] Status bar shows correct total count
- [ ] No memory leaks (scroll up/down rapidly, check heap in DevTools)
- [ ] Backend cache expires after 30 seconds
- [ ] New search invalidates previous cache
