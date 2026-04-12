import { useState, useMemo, useRef, useEffect } from 'react';

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
  isSearching: boolean;
  onOpenFile: (path: string) => void;
  onOpenFolder: (path: string) => void;
  onDeleteFile?: (path: string) => void;
  onExport: (format: 'csv' | 'txt' | 'json') => void;
  pagination?: {
    page: number;
    total: number;
    total_pages: number;
    onPageChange: (page: number) => void;
  };
}

type SortField = 'name' | 'size' | 'modified_time';
type SortDirection = 'asc' | 'desc';

function ResultList({ results, isSearching, onOpenFile, onOpenFolder, onDeleteFile, onExport, pagination }: ResultListProps) {
  const [sortField, setSortField] = useState<SortField>('name');
  const [sortDirection, setSortDirection] = useState<SortDirection>('asc');
  const [selectedIndex, setSelectedIndex] = useState(-1);
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; path: string; isDirectory: boolean } | null>(null);
  const [hoveredItem, setHoveredItem] = useState<{ index: number; x: number; y: number; data: SearchResult } | null>(null);
  const [showTooltip, setShowTooltip] = useState(false);
  const resultBodyRef = useRef<HTMLDivElement>(null);
  const hoverTimeoutRef = useRef<number | null>(null);
  const ROW_HEIGHT = 24;

  // 清理定时器
  useEffect(() => {
    return () => {
      if (hoverTimeoutRef.current) {
        clearTimeout(hoverTimeoutRef.current);
      }
    };
  }, []);

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
      setSelectedIndex(prev => {
        const newIndex = Math.min(prev + 1, sortedResults.length - 1);
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
      if (resultBodyRef.current) resultBodyRef.current.scrollTop = 0;
    } else if (e.key === 'End') {
      e.preventDefault();
      const lastIndex = sortedResults.length - 1;
      setSelectedIndex(lastIndex);
      if (resultBodyRef.current) resultBodyRef.current.scrollTop = resultBodyRef.current.scrollHeight;
    } else if (e.key === 'PageDown') {
      e.preventDefault();
      if (resultBodyRef.current) {
        const newIndex = Math.min(selectedIndex + Math.floor(resultBodyRef.current.clientHeight / ROW_HEIGHT), sortedResults.length - 1);
        setSelectedIndex(newIndex);
        scrollToIndex(newIndex);
      }
    } else if (e.key === 'PageUp') {
      e.preventDefault();
      if (resultBodyRef.current) {
        const newIndex = Math.max(selectedIndex - Math.floor(resultBodyRef.current.clientHeight / ROW_HEIGHT), 0);
        setSelectedIndex(newIndex);
        scrollToIndex(newIndex);
      }
    } else if (e.key === 'Enter' && selectedIndex >= 0) {
      const item = sortedResults[selectedIndex];
      handleRowDoubleClick(item.path, item.is_directory);
    }
  };

  const getSortIcon = (field: SortField) => {
    if (sortField !== field) return '';
    return sortDirection === 'asc' ? ' ▲' : ' ▼';
  };

  const handleContextMenu = (e: React.MouseEvent, path: string, isDirectory: boolean) => {
    e.preventDefault();
    // 右键点击时立即关闭提示
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
    
    // 清除之前的定时器
    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
    }
    
    // 500毫秒后显示提示
    hoverTimeoutRef.current = setTimeout(() => {
      setShowTooltip(true);
    }, 500);
  };

  const handleMouseLeave = () => {
    // 清除定时器并隐藏提示
    if (hoverTimeoutRef.current) {
      clearTimeout(hoverTimeoutRef.current);
      hoverTimeoutRef.current = null;
    }
    setHoveredItem(null);
    setShowTooltip(false);
  };

  return (
    <div className="result-list" tabIndex={0} onKeyDown={handleKeyDown} onClick={closeContextMenu}>
      <div className="result-header">
        <div className="result-left">
          <div className="result-count">
            {isSearching ? '搜索中...' : `${results.length} 个结果`}
          </div>
          {pagination && pagination.total_pages > 1 && (
            <div className="pagination">
              <button 
                disabled={pagination.page <= 1}
                onClick={() => pagination.onPageChange(1)}
              >第一页</button>
              <button 
                disabled={pagination.page <= 1}
                onClick={() => pagination.onPageChange(pagination.page - 1)}
              >上一页</button>
              <span>
                <input 
                  type="number" 
                  min={1}
                  max={pagination.total_pages}
                  defaultValue={pagination.page}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      const page = parseInt((e.target as HTMLInputElement).value);
                      if (page >= 1 && page <= pagination.total_pages) {
                        pagination.onPageChange(page);
                      }
                    }
                  }}
                  onBlur={(e) => {
                    const page = parseInt((e.target as HTMLInputElement).value);
                    if (page >= 1 && page <= pagination.total_pages) {
                      pagination.onPageChange(page);
                    }
                  }}
                  className="page-input"
                />
                / {pagination.total_pages}
              </span>
              <button 
                disabled={pagination.page >= pagination.total_pages}
                onClick={() => pagination.onPageChange(pagination.page + 1)}
              >下一页</button>
              <button 
                disabled={pagination.page >= pagination.total_pages}
                onClick={() => pagination.onPageChange(pagination.total_pages)}
              >最后一页</button>
            </div>
          )}
        </div>
        <div className="export-buttons">
          <select onChange={(e) => {
            const format = e.target.value;
            if (format) onExport(format as 'csv' | 'txt' | 'json');
          }} defaultValue="">
            <option value="" disabled>导出...</option>
            <option value="csv">CSV</option>
            <option value="txt">TXT</option>
            <option value="json">JSON</option>
          </select>
        </div>
      </div>
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
          {sortedResults.map((result, index) => (
            <div
              key={result.file_id}
              className={`result-row ${index === selectedIndex ? 'selected' : ''}`}
              onClick={() => handleRowClick(index)}
              onDoubleClick={() => handleRowDoubleClick(result.path, result.is_directory)}
              onContextMenu={(e) => handleContextMenu(e, result.path, result.is_directory)}
              onMouseEnter={(e) => handleMouseEnter(e, index, result)}
              onMouseLeave={handleMouseLeave}
            >
              <div className="col-name">
                <span className="file-icon">{result.is_directory ? '📁' : '📄'}</span>
                {result.name}
              </div>
              <div className="col-path" title={result.path}>{result.path}</div>
              <div className="col-size">{result.formatted_size}</div>
              <div className="col-modified">{result.formatted_modified_time}</div>
            </div>
          ))}
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
          <div className="hover-tooltip-row"><strong>大小:</strong> {hoveredItem.data.formatted_size}</div>
          <div className="hover-tooltip-row"><strong>日期:</strong> {hoveredItem.data.formatted_modified_time}</div>
          <div className="hover-tooltip-row"><strong>路径:</strong> {hoveredItem.data.path}</div>
        </div>
      )}
    </div>
  );
}

export default ResultList;