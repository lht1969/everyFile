import { useState, useRef, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useVirtualScroll } from '../hooks/useVirtualScroll';
import { useFileIcon } from '../hooks/useFileIcon';
import type { SearchResult, SortField, SortDirection } from '../types';
import { formatSize, formatTime } from '../utils/format';

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
}

const ROW_HEIGHT = 28;

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

function ResultList({ results, totalCount, resultsOffset, sortField, sortDirection, onOpenFile, onOpenFolder, onVisibleRangeChange, onSortChange, scrollToTop, searching }: ResultListProps) {
  const [selectedIndex, setSelectedIndex] = useState(-1);
  const [hoveredItem, setHoveredItem] = useState<{ index: number; x: number; y: number; data: SearchResult } | null>(null);
  const [showTooltip, setShowTooltip] = useState(false);
  const [colWidths, setColWidths] = useState(['1fr', '2fr', '100px', '150px']);
  const resultBodyRef = useRef<HTMLDivElement>(null);
  const hoverTimeoutRef = useRef<number | null>(null);
  const resizingRef = useRef<{ colIndex: number; startX: number; startWidth: number } | null>(null);

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
    return () => { if (hoverTimeoutRef.current) clearTimeout(hoverTimeoutRef.current); };
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

  const handleRowClick = (index: number) => setSelectedIndex(index);

  const handleRowDoubleClick = (path: string, isDirectory: boolean) => {
    isDirectory ? onOpenFolder(path) : onOpenFile(path);
  };

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
      if (item) handleRowDoubleClick(item.path, item.is_directory);
    }
  };

  const getSortIcon = (field: SortField) => sortField !== field ? '' : (sortDirection === 'asc' ? ' ▲' : ' ▼');

  const handleContextMenu = (e: React.MouseEvent, path: string) => {
    // 阻止浏览器/WebView 默认菜单
    e.preventDefault();
    setShowTooltip(false);
    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
      hoverTimeoutRef.current = null;
    }
    // 屏幕坐标 = 视口坐标 + 窗口偏移
    // Tauri WebView2 顶层窗口的 window.screenX/Y 即窗口在屏幕上的位置
    const screenX = e.clientX + (window.screenX || 0);
    const screenY = e.clientY + (window.screenY || 0);
    // 调用 Rust 端 show_context_menu，弹出系统 Shell 菜单
    // 系统菜单会自动显示"打开/打开方式/属性/删除到回收站"等所有原生项
    invoke('show_context_menu', { path, screenX, screenY }).catch(err => {
      console.error('Failed to show context menu:', err);
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
      const newWidth = Math.max(50, resizingRef.current.startWidth + (me.clientX - resizingRef.current.startX));
      setColWidths(prev => { const next = [...prev]; next[resizingRef.current!.colIndex] = newWidth + 'px'; return next; });
    };
    const onUp = () => { resizingRef.current = null; document.removeEventListener('mousemove', onMove); document.removeEventListener('mouseup', onUp); };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  };

  return (
    <div className="result-list" tabIndex={0} onKeyDown={handleKeyDown}>
      <div className="result-table">
        <div className="result-row header" style={{ gridTemplateColumns: colWidths.join(' ') }}>
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
            const renderLen = Math.max(0, renderEnd - startIndex);
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
                        style={{ height: ROW_HEIGHT, gridTemplateColumns: colWidths.join(' ') }}
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
                      style={{ height: ROW_HEIGHT, gridTemplateColumns: colWidths.join(' ') }}
                      onClick={() => handleRowClick(globalIndex)}
                      onDoubleClick={() => handleRowDoubleClick(result.path, result.is_directory)}
                      onContextMenu={(e) => handleContextMenu(e, result.path)}
                      onMouseEnter={(e) => handleMouseEnter(e, globalIndex, result)}
                      onMouseLeave={handleMouseLeave}
                    >
                      <div className="col-name">
                        <FileIcon path={result.path} isDirectory={result.is_directory} />
                        <span className="col-name-text" title={result.name}>{result.name}</span>
                      </div>
                      <div className="col-path" title={result.path}>{getDirectoryPath(result.path, result.is_directory)}</div>
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
    </div>
  );
}

export default ResultList;
