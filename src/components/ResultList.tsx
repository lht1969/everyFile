import { useState, useRef, useEffect } from 'react';
import { useVirtualScroll } from '../hooks/useVirtualScroll';

interface SearchResult {
  file_id: number;
  name: string;
  path: string;
  size: number;
  modified_time: number;
  is_directory: boolean;
}

function formatSize(bytes: number): string {
  if (bytes >= 1073741824) return (bytes / 1073741824).toFixed(1) + ' GB';
  if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + ' MB';
  if (bytes >= 1024) return (bytes / 1024).toFixed(1) + ' KB';
  return bytes + ' B';
}

function formatTime(ts: number): string {
  const d = new Date(ts * 1000);
  const pad = (n: number) => n.toString().padStart(2, '0');
  return `${d.getFullYear()}/${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

interface ResultListProps {
  results: SearchResult[];
  totalCount: number;
  onOpenFile: (path: string) => void;
  onOpenFolder: (path: string) => void;
  onDeleteFile?: (path: string) => void;
  onVisibleRangeChange?: (startIndex: number, endIndex: number) => void;
  onSortChange?: (field: SortField, direction: SortDirection) => void;
  scrollToTop?: number;
}

type SortField = 'name' | 'size' | 'modified_time' | 'path';
type SortDirection = 'asc' | 'desc';

const ROW_HEIGHT = 28;

function ResultList({ results, totalCount, onOpenFile, onOpenFolder, onDeleteFile, onVisibleRangeChange, onSortChange, scrollToTop }: ResultListProps) {
  const [sortField, setSortField] = useState<SortField>('name');
  const [sortDirection, setSortDirection] = useState<SortDirection>('asc');
  const [selectedIndex, setSelectedIndex] = useState(-1);
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; path: string; isDirectory: boolean } | null>(null);
  const [hoveredItem, setHoveredItem] = useState<{ index: number; x: number; y: number; data: SearchResult } | null>(null);
  const [showTooltip, setShowTooltip] = useState(false);
  const resultBodyRef = useRef<HTMLDivElement>(null);
  const hoverTimeoutRef = useRef<number | null>(null);

  const { startIndex, offsetY, spacerHeight, scrollToIndex } = useVirtualScroll({
    totalItems: totalCount,
    itemHeight: ROW_HEIGHT,
    overscan: 5,
    containerRef: resultBodyRef,
    onRangeChange: onVisibleRangeChange,
  });

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
      if (hoverTimeoutRef.current) {
        clearTimeout(hoverTimeoutRef.current);
      }
    };
  }, []);

  useEffect(() => {
    if (scrollToTop !== undefined && resultBodyRef.current) {
      resultBodyRef.current.scrollTop = 0;
    }
  }, [scrollToTop]);

  const handleSort = (field: SortField) => {
    let newDirection: SortDirection;
    if (field === sortField) {
      newDirection = sortDirection === 'asc' ? 'desc' : 'asc';
    } else {
      newDirection = 'asc';
    }
    setSortField(field);
    setSortDirection(newDirection);
    if (onSortChange) {
      onSortChange(field, newDirection);
    }
  };

  const handleRowClick = (index: number) => {
    setSelectedIndex(index);
  };

  const handleRowDoubleClick = (path: string, isDirectory: boolean) => {
    if (isDirectory) {
      onOpenFolder(path);
    } else {
      onOpenFile(path);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setSelectedIndex(prev => {
        const newIndex = Math.min(prev + 1, totalCount - 1);
        scrollToIndex(newIndex);
        return newIndex;
      });
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setSelectedIndex(prev => {
        const newIndex = Math.max(prev - 1, 0);
        scrollToIndex(newIndex);
        return newIndex;
      });
    } else if (e.key === 'Home') {
      e.preventDefault();
      setSelectedIndex(0);
      scrollToIndex(0);
    } else if (e.key === 'End') {
      e.preventDefault();
      const lastIndex = totalCount - 1;
      setSelectedIndex(lastIndex);
      scrollToIndex(lastIndex);
    } else if (e.key === 'PageDown') {
      e.preventDefault();
      if (resultBodyRef.current) {
        const step = Math.floor(resultBodyRef.current.clientHeight / ROW_HEIGHT);
        const newIndex = Math.min(selectedIndex + step, totalCount - 1);
        setSelectedIndex(newIndex);
        scrollToIndex(newIndex);
      }
    } else if (e.key === 'PageUp') {
      e.preventDefault();
      if (resultBodyRef.current) {
        const step = Math.floor(resultBodyRef.current.clientHeight / ROW_HEIGHT);
        const newIndex = Math.max(selectedIndex - step, 0);
        setSelectedIndex(newIndex);
        scrollToIndex(newIndex);
      }
    } else if (e.key === 'Enter' && selectedIndex >= 0) {
      const item = results.find((_, i) => i === selectedIndex - startIndex);
      if (item) {
        handleRowDoubleClick(item.path, item.is_directory);
      }
    }
  };

  const getSortIcon = (field: SortField) => {
    if (sortField !== field) return '';
    return sortDirection === 'asc' ? ' ▲' : ' ▼';
  };

  const handleContextMenu = (e: React.MouseEvent, path: string, isDirectory: boolean) => {
    e.preventDefault();
    setShowTooltip(false);
    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
      hoverTimeoutRef.current = null;
    }
    setContextMenu({ x: e.clientX, y: e.clientY, path, isDirectory });
  };

  const closeContextMenu = () => setContextMenu(null);

  const handleMouseEnter = (e: React.MouseEvent, index: number, data: SearchResult) => {
    const rect = (e.target as HTMLElement).getBoundingClientRect();
    setHoveredItem({ index, x: rect.left, y: rect.bottom, data });

    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
    }

    hoverTimeoutRef.current = setTimeout(() => {
      setShowTooltip(true);
    }, 500);
  };

  const handleMouseLeave = () => {
    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
      hoverTimeoutRef.current = null;
    }
    setHoveredItem(null);
    setShowTooltip(false);
  };

  return (
    <div className="result-list" tabIndex={0} onKeyDown={handleKeyDown} onClick={closeContextMenu}>
      <div className="result-table">
        <div className="result-row header">
          <div className="col-name" onClick={() => handleSort('name')}>
            名称{getSortIcon('name')}
          </div>
          <div className="col-path" onClick={() => handleSort('path')}>
            路径{getSortIcon('path')}</div>
          <div className="col-size" onClick={() => handleSort('size')}>
            大小{getSortIcon('size')}
          </div>
          <div className="col-modified" onClick={() => handleSort('modified_time')}>
            修改时间{getSortIcon('modified_time')}
          </div>
        </div>
        <div className="result-body" ref={resultBodyRef}>
          <div className="virtual-spacer" style={{ height: spacerHeight }} />
          <div className="virtual-content" style={{ transform: `translateY(${offsetY}px)` }}>
            {results.map((result, i) => {
              const globalIndex = startIndex + i;
              return (
                <div
                  key={result.path}
                  className={`result-row ${globalIndex === selectedIndex ? 'selected' : ''}`}
                  style={{ height: ROW_HEIGHT }}
                  onClick={() => handleRowClick(globalIndex)}
                  onDoubleClick={() => handleRowDoubleClick(result.path, result.is_directory)}
                  onContextMenu={(e) => handleContextMenu(e, result.path, result.is_directory)}
                  onMouseEnter={(e) => handleMouseEnter(e, globalIndex, result)}
                  onMouseLeave={handleMouseLeave}
                >
                  <div className="col-name">
                    <span className="file-icon">{result.is_directory ? '📁' : '📄'}</span>
                    {result.name}
                  </div>
                  <div className="col-path" title={result.path}>{getDirectoryPath(result.path, result.is_directory)}</div>
                  <div className="col-size">{formatSize(result.size)}</div>
                  <div className="col-modified">{formatTime(result.modified_time)}</div>
                </div>
              );
            })}
          </div>
        </div>
      </div>
      {contextMenu && (
        <div
          className="context-menu"
          style={{ left: contextMenu.x, top: contextMenu.y }}
          onClick={(e) => e.stopPropagation()}
        >
          <div className="context-menu-item" onClick={() => { onOpenFile(contextMenu.path); closeContextMenu(); }}>
            打开
          </div>
          <div className="context-menu-item" onClick={() => { onOpenFolder(contextMenu.path); closeContextMenu(); }}>
            打开文件夹
          </div>
          <div className="context-menu-item" onClick={() => { navigator.clipboard.writeText(contextMenu.path); closeContextMenu(); }}>
            复制路径
          </div>
          {onDeleteFile && (
            <div className="context-menu-item danger" onClick={() => { onDeleteFile(contextMenu.path); closeContextMenu(); }}>
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
