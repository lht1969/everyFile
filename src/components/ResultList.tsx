import { useState, useRef, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useVirtualScroll } from '../hooks/useVirtualScroll';
import { useFileIcon } from '../hooks/useFileIcon';
import { useColumnWidths } from '../hooks/useColumnWidths';
import type { SearchResult, SortField, SortDirection } from '../types';
import { formatSize, formatTime } from '../utils/format';
import { highlightMatch } from '../utils/highlight';

interface ResultListProps {
  results: SearchResult[];
  resultsOffset: number;
  totalCount: number;
  sortField: SortField;
  sortDirection: SortDirection;
  onOpenFile: (path: string) => void;
  onOpenFolder: (path: string) => void;
  onVisibleRangeChange?: (startIndex: number, endIndex: number) => void;
  onSortChange?: (field: SortField, direction: SortDirection) => void;
  scrollToTop?: number;
  searching?: boolean;
  searchQuery?: string;
}

const ROW_HEIGHT = 28;
const DOUBLE_CLICK_MS = 500;

function FileIcon({ path, isDirectory }: { path: string; isDirectory: boolean }) {
  const iconUrl = useFileIcon(path, isDirectory);

  if (iconUrl === undefined) {
    return <span className="file-icon file-icon-placeholder" />;
  }

  if (iconUrl === '') {
    return <span className="file-icon">{isDirectory ? '📁' : '📄'}</span>;
  }

  return <img className="file-icon-img" src={iconUrl} alt="" draggable={false} />;
}

function ResultList({ results, totalCount, resultsOffset, sortField, sortDirection, onOpenFile, onOpenFolder, onVisibleRangeChange, onSortChange, scrollToTop, searching, searchQuery }: ResultListProps) {
  const [selectedIndex, setSelectedIndex] = useState(-1);
  const [hoveredItem, setHoveredItem] = useState<{ index: number; x: number; y: number; data: SearchResult } | null>(null);
  const [showTooltip, setShowTooltip] = useState(false);
  const [renamingIndex, setRenamingIndex] = useState<number | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [conflictInfo, setConflictInfo] = useState<{ oldPath: string; newName: string; existingPath: string } | null>(null);
  const resultBodyRef = useRef<HTMLDivElement>(null);
  const hoverTimeoutRef = useRef<number | null>(null);
  const resizingRef = useRef<{ colIndex: number; startX: number; startWidth: number } | null>(null);
  const renameInputRef = useRef<HTMLInputElement>(null);
  const pendingRenameRef = useRef<{ index: number; timerId: number } | null>(null);

  const { columnWidths, setManualWidth } = useColumnWidths(results, resultBodyRef);
  const gridTemplate = columnWidths.map(w => w + 'px').join(' ');

  const { startIndex, endIndex, offsetY, spacerHeight, scrollToIndex, resetScroll } = useVirtualScroll({
    totalItems: totalCount,
    itemHeight: ROW_HEIGHT,
    overscan: 5,
    containerRef: resultBodyRef,
    onRangeChange: onVisibleRangeChange,
  });

  const getDirectoryPath = (path: string, isDirectory: boolean): string => {
    if (isDirectory) return path.endsWith('\\') ? path : path + '\\';
    const lastBackslash = path.lastIndexOf('\\');
    return lastBackslash > 0 ? path.substring(0, lastBackslash) : path;
  };

  useEffect(() => {
    return () => {
      if (hoverTimeoutRef.current) clearTimeout(hoverTimeoutRef.current);
      if (pendingRenameRef.current) clearTimeout(pendingRenameRef.current.timerId);
    };
  }, []);

  const prevScrollTrigger = useRef(scrollToTop);
  useEffect(() => {
    if (scrollToTop !== undefined && scrollToTop !== prevScrollTrigger.current) {
      prevScrollTrigger.current = scrollToTop;
      resetScroll();
    }
  }, [scrollToTop, resetScroll]);

  const handleSort = (field: SortField) => {
    const newDirection = field === sortField ? (sortDirection === 'asc' ? 'desc' : 'asc') : 'asc';
    onSortChange?.(field, newDirection);
  };

  const commitRename = async (index: number, force?: boolean) => {
    const result = results[index - resultsOffset];
    if (!result || !renameValue.trim() || renameValue === result.name) {
      setRenamingIndex(null);
      return;
    }
    try {
      const resp = await invoke<{ status: string; newPath?: string; existingPath?: string }>('rename_file', {
        oldPath: result.path,
        newName: renameValue.trim(),
        force: force ?? false,
      });
      if (resp.status === 'ok') {
        result.name = renameValue.trim();
        result.path = resp.newPath!;
        setRenamingIndex(null);
      } else if (resp.status === 'conflict') {
        setConflictInfo({ oldPath: result.path, newName: renameValue.trim(), existingPath: resp.existingPath! });
      }
    } catch (err) {
      console.error('[RENAME] rename_file FAILED:', err);
      setRenamingIndex(null);
    }
  };

  const cancelRename = () => { setRenamingIndex(null); setConflictInfo(null); };

  const handleConflictOverwrite = () => {
    if (conflictInfo && renamingIndex !== null) {
      setConflictInfo(null);
      commitRename(renamingIndex, true);
    }
  };

  const handleConflictAutoRename = async () => {
    if (conflictInfo && renamingIndex !== null) {
      const result = results[renamingIndex - resultsOffset];
      if (result) {
        setConflictInfo(null);
        const ext = result.name.lastIndexOf('.');
        const base = ext > 0 ? result.name.substring(0, ext) : result.name;
        const suffix = ext > 0 ? result.name.substring(ext) : '';
        for (let n = 2; n <= 99; n++) {
          const candidate = `${base} (${n})${suffix}`;
          if (candidate === result.name) continue;
          try {
            const resp = await invoke<{ status: string; newPath?: string; existingPath?: string }>('rename_file', {
              oldPath: result.path,
              newName: candidate,
              force: false,
            });
            if (resp.status === 'ok') {
              result.name = candidate;
              result.path = resp.newPath!;
              setRenamingIndex(null);
              return;
            }
            if (resp.status === 'conflict') continue;
          } catch { break; }
        }
        setRenamingIndex(null);
      }
    }
  };

  const cancelPendingRename = () => {
    if (pendingRenameRef.current) {
      clearTimeout(pendingRenameRef.current.timerId);
      pendingRenameRef.current = null;
    }
  };

  const startRename = (index: number, name: string) => {
    setRenamingIndex(index);
    setRenameValue(name);
  };

  // 延迟进入重命名状态：双击会依次触发 click → click → dblclick，
  // 首次 click 无法区分"单击改名"与"双击的第一步"。延迟 DOUBLE_CLICK_MS
  // 后若没有 dblclick 到来才进入重命名；dblclick 到达时取消待定的重命名。
  const scheduleRename = (index: number, name: string) => {
    cancelPendingRename();
    const timerId = window.setTimeout(() => {
      pendingRenameRef.current = null;
      startRename(index, name);
    }, DOUBLE_CLICK_MS);
    pendingRenameRef.current = { index, timerId };
  };

  const handleRowClick = (index: number, e: React.MouseEvent) => {
    cancelPendingRename();
    if (selectedIndex === index && renamingIndex === null) {
      const target = e.target as HTMLElement;
      if (target.closest('.col-name')) {
        const result = results[index - resultsOffset];
        if (result) {
          scheduleRename(index, result.name);
          return;
        }
      }
    }
    if (renamingIndex !== null && renamingIndex !== index) {
      commitRename(renamingIndex);
    }
    setSelectedIndex(index);
  };

  const handleRowDoubleClick = (index: number, path: string, isDirectory: boolean) => {
    if (renamingIndex === index) return;
    cancelPendingRename();
    isDirectory ? onOpenFolder(path) : onOpenFile(path);
  };

  useEffect(() => {
    if (renamingIndex !== null && renameInputRef.current) {
      const input = renameInputRef.current;
      input.focus();
      const dotIndex = renameValue.lastIndexOf('.');
      input.setSelectionRange(0, dotIndex > 0 ? dotIndex : renameValue.length);
    }
  }, [renamingIndex]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setSelectedIndex(prev => { const n = Math.min(prev + 1, totalCount - 1); scrollToIndex(n); return n; });
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setSelectedIndex(prev => { const n = Math.max(prev - 1, 0); scrollToIndex(n); return n; });
    } else if (e.key === 'Home') {
      e.preventDefault(); setSelectedIndex(0); scrollToIndex(0);
    } else if (e.key === 'End') {
      e.preventDefault(); const n = totalCount - 1; setSelectedIndex(n); scrollToIndex(n);
    } else if (e.key === 'PageDown') {
      e.preventDefault();
      if (resultBodyRef.current) {
        const step = Math.floor(resultBodyRef.current.clientHeight / ROW_HEIGHT);
        const n = Math.min(selectedIndex + step, totalCount - 1); setSelectedIndex(n); scrollToIndex(n);
      }
    } else if (e.key === 'PageUp') {
      e.preventDefault();
      if (resultBodyRef.current) {
        const step = Math.floor(resultBodyRef.current.clientHeight / ROW_HEIGHT);
        const n = Math.max(selectedIndex - step, 0); setSelectedIndex(n); scrollToIndex(n);
      }
    } else if (e.key === 'Enter' && selectedIndex >= 0) {
      const item = results.find((_, i) => i === selectedIndex - startIndex);
      if (item) handleRowDoubleClick(selectedIndex, item.path, item.is_directory);
    }
  };

  const getSortIcon = (field: SortField) => sortField !== field ? '' : (sortDirection === 'asc' ? ' ▲' : ' ▼');

  const handleContextMenu = (e: React.MouseEvent, path: string, index: number) => {
    e.preventDefault();
    setSelectedIndex(index);
    setShowTooltip(false);
    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
      hoverTimeoutRef.current = null;
    }
    // 屏幕坐标 = 视口坐标 + 窗口偏移
    const screenX = e.clientX + (window.screenX || 0);
    const screenY = e.clientY + (window.screenY || 0);
    invoke('show_context_menu', { path, screenX, screenY })
      .catch(err => {
        console.error('[CTX_MENU] show_context_menu FAILED:', err);
      });
  };

  const handleMouseEnter = (e: React.MouseEvent, index: number, data: SearchResult) => {
    const rect = (e.target as HTMLElement).getBoundingClientRect();
    setHoveredItem({ index, x: rect.left, y: rect.bottom, data });
    if (hoverTimeoutRef.current) clearTimeout(hoverTimeoutRef.current);
    hoverTimeoutRef.current = setTimeout(() => setShowTooltip(true), 500);
  };

  const handleMouseLeave = () => {
    if (hoverTimeoutRef.current) { clearTimeout(hoverTimeoutRef.current); hoverTimeoutRef.current = null; }
    setHoveredItem(null); setShowTooltip(false);
  };

  const handleResizeStart = (e: React.MouseEvent, colIndex: number) => {
    e.preventDefault(); e.stopPropagation();
    const startWidth = e.currentTarget.parentElement?.getBoundingClientRect().width ?? 0;
    resizingRef.current = { colIndex, startX: e.clientX, startWidth };
    const onMove = (me: MouseEvent) => {
      if (!resizingRef.current) return;
      const newWidth = Math.max(100, resizingRef.current.startWidth + (me.clientX - resizingRef.current.startX));
      setManualWidth(resizingRef.current.colIndex, newWidth);
    };
    const onUp = () => { resizingRef.current = null; document.removeEventListener('mousemove', onMove); document.removeEventListener('mouseup', onUp); };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  };

  return (
    <div className="result-list" tabIndex={0} onKeyDown={handleKeyDown}>
      <div className="result-table">
        <div className="result-row header" style={{ gridTemplateColumns: gridTemplate }}>
          <div className="col-name" onClick={() => handleSort('name')}>
            名称{getSortIcon('name')}
            <div className="col-resize" onMouseDown={(e) => handleResizeStart(e, 0)} />
          </div>
          <div className="col-path" onClick={() => handleSort('path')}>
            路径{getSortIcon('path')}
            <div className="col-resize" onMouseDown={(e) => handleResizeStart(e, 1)} />
          </div>
          <div className="col-size" onClick={() => handleSort('size')}>
            大小{getSortIcon('size')}
            <div className="col-resize" onMouseDown={(e) => handleResizeStart(e, 2)} />
          </div>
          <div className="col-modified" onClick={() => handleSort('modified_time')}>
            修改时间{getSortIcon('modified_time')}
            <div className="col-resize" onMouseDown={(e) => handleResizeStart(e, 3)} />
          </div>
        </div>
        <div className="result-body" ref={resultBodyRef}>
          {results.length === 0 && totalCount === 0 && !searching && (
            <div className="empty-state">
              <div className="empty-icon">🔍</div>
              <div className="empty-text">输入关键词开始搜索</div>
            </div>
          )}
          {results.length === 0 && totalCount === 0 && searching && (
            <div className="empty-state">
              <div className="empty-spinner">⟳</div>
              <div className="empty-text">搜索中...</div>
            </div>
          )}
          {results.length === 0 && totalCount > 0 && (
            <div className="empty-state">
              <div className="empty-icon">📭</div>
              <div className="empty-text">未找到匹配结果</div>
            </div>
          )}
          <div className="virtual-spacer" style={{ height: spacerHeight }} />
          {(() => {
            const dataEnd = resultsOffset + results.length;
            const renderEnd = Math.min(endIndex, dataEnd);
            const atBottom = endIndex >= totalCount;
            // 底部时确保至少渲染视口内可见行数，但不能超过当前可用数据量，
            // 避免在结果集较小（如 7 条）且 endIndex 包含大量 overscan 时，
            // 渲染过多空白 placeholder 行，被用户感知为"空白窗口"。
            const bodyHeight = resultBodyRef.current?.clientHeight ?? 0;
            const visibleInView = Math.max(1, Math.ceil(bodyHeight / ROW_HEIGHT));
            const minRenderLen = atBottom
              ? Math.min(visibleInView, totalCount - startIndex)
              : 0;
            const renderLen = Math.max(Math.max(0, renderEnd - startIndex), minRenderLen);
            return (
              <div className="virtual-content" style={{ transform: `translateY(${offsetY}px)`, height: renderLen * ROW_HEIGHT }}>
                {Array.from({ length: renderLen }, (_, i) => {
                  const globalIndex = startIndex + i;
                  const result = results[globalIndex - resultsOffset];
                  if (!result) {
                    return (
                      <div
                        key={`placeholder-${globalIndex}`}
                        className="result-row"
                        style={{ height: ROW_HEIGHT, gridTemplateColumns: gridTemplate }}
                      />
                    );
                  }
                  return (
                    <div
                      // 用 globalIndex 配合 result.path 作为 key，确保唯一性
                      // 原因：不同排序时相同 path 在 results 中可能重复出现（如某次 0-50 区间里有它，
                      // 下次排序后还在 0-50 区间里），仅用 result.path 会导致 React 复用错误的 DOM 节点，
                      // 表现为"某些行固定不跟着排序"（其实是 DOM 节点没被替换，只是 props 改了）
                      key={`${globalIndex}-${result.path}`}
                      className={`result-row ${globalIndex === selectedIndex ? 'selected' : ''}`}
                      style={{ height: ROW_HEIGHT, gridTemplateColumns: gridTemplate }}
                      onClick={(e) => handleRowClick(globalIndex, e)}
                      onDoubleClick={() => handleRowDoubleClick(globalIndex, result.path, result.is_directory)}
                      onMouseEnter={(e) => handleMouseEnter(e, globalIndex, result)}
                      onMouseLeave={handleMouseLeave}
                    >
                      <div className="col-name" onContextMenu={(e) => handleContextMenu(e, result.path, globalIndex)}>
                        {renamingIndex === globalIndex ? (
                          <input
                            ref={renameInputRef}
                            className="rename-input"
                            value={renameValue}
                            onChange={e => setRenameValue(e.target.value)}
                            onBlur={() => commitRename(globalIndex)}
                            onKeyDown={e => {
                              if (e.key === 'Enter') { e.preventDefault(); commitRename(globalIndex); }
                              else if (e.key === 'Escape') { e.preventDefault(); cancelRename(); }
                              e.stopPropagation();
                            }}
                            onClick={e => e.stopPropagation()}
                          />
                        ) : (
                          <>
                            <FileIcon path={result.path} isDirectory={result.is_directory} />
                            <span className="col-name-text">{highlightMatch(result.name, searchQuery || '')}</span>
                          </>
                        )}
                      </div>
                      <div className="col-path" onContextMenu={(e) => handleContextMenu(e, getDirectoryPath(result.path, result.is_directory), globalIndex)}>{highlightMatch(getDirectoryPath(result.path, result.is_directory), searchQuery || '')}</div>
                      <div className="col-size">{formatSize(result.size)}</div>
                      <div className="col-modified">{formatTime(result.modified_time)}</div>
                    </div>
                  );
                })}
              </div>
            );
          })()}
        </div>
      </div>

      {hoveredItem && showTooltip && (
        <div className="hover-tooltip" style={{ left: hoveredItem.x, top: hoveredItem.y }}>
          <div className="hover-tooltip-row"><strong>名称:</strong> {hoveredItem.data.name}</div>
          <div className="hover-tooltip-row"><strong>大小:</strong> {formatSize(hoveredItem.data.size)}</div>
          <div className="hover-tooltip-row"><strong>日期:</strong> {formatTime(hoveredItem.data.modified_time)}</div>
          <div className="hover-tooltip-row"><strong>路径:</strong> {hoveredItem.data.path}</div>
        </div>
      )}

      {conflictInfo && (
        <div className="modal-overlay" onClick={cancelRename}>
          <div className="modal-content" onClick={e => e.stopPropagation()} style={{ maxWidth: 420 }}>
            <div className="modal-header">
              <h2>文件名冲突</h2>
            </div>
            <div className="modal-body">
              <p>目标文件已存在：</p>
              <p style={{ margin: '8px 0', padding: '6px 10px', background: '#f3f4f6', borderRadius: 4, fontSize: 13, wordBreak: 'break-all' }}>
                {conflictInfo.existingPath}
              </p>
            </div>
            <div className="modal-footer" style={{ gap: 8, justifyContent: 'flex-end' }}>
              <button className="btn btn-secondary" onClick={cancelRename}>取消</button>
              <button className="btn btn-secondary" onClick={handleConflictAutoRename}>自动重命名</button>
              <button className="btn btn-primary" onClick={handleConflictOverwrite}>替换目标文件</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default ResultList;
