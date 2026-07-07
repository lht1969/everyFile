import { useState, useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface SearchPanelProps {
  onSearch: (query: string, filesOnly?: boolean, directoriesOnly?: boolean) => void;
  onOpenSettings: () => void;
  onExport?: (format: 'csv' | 'txt' | 'json') => void;
}

function SearchPanel({ onSearch, onOpenSettings, onExport }: SearchPanelProps) {
  const [query, setQuery] = useState('');
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const [filterType, setFilterType] = useState<'files' | 'directories'>('files');
  const [exportFormat, setExportFormat] = useState('');
  const [helpVisible, setHelpVisible] = useState(false);
  const prevQueryRef = useRef(query);
  const prevFilterRef = useRef(filterType);
  const helpRef = useRef<HTMLDivElement>(null);
  const helpBtnRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const prevQuery = prevQueryRef.current;
    const prevFilter = prevFilterRef.current;
    prevQueryRef.current = query;
    prevFilterRef.current = filterType;

    const filterChanged = prevFilter !== filterType;

    if (prevQuery.trim() && !query.trim()) {
      setSuggestions([]);
      onSearch('', filterType === 'files', filterType === 'directories');
      return;
    }
    if (!query.trim()) {
      setSuggestions([]);
      if (filterChanged) {
        onSearch('', filterType === 'files', filterType === 'directories');
      }
      return;
    }
    const debounce = setTimeout(() => {
      onSearch(query, filterType === 'files', filterType === 'directories');
      fetchSuggestions(query);
    }, 150);
    return () => clearTimeout(debounce);
  }, [query, filterType]);

  useEffect(() => {
    if (!helpVisible) return;
    const handler = (e: MouseEvent) => {
      if (
        helpRef.current &&
        !helpRef.current.contains(e.target as Node) &&
        helpBtnRef.current &&
        !helpBtnRef.current.contains(e.target as Node)
      ) {
        setHelpVisible(false);
      }
    };
    const keyHandler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setHelpVisible(false);
    };
    document.addEventListener('mousedown', handler);
    document.addEventListener('keydown', keyHandler);
    return () => {
      document.removeEventListener('mousedown', handler);
      document.removeEventListener('keydown', keyHandler);
    };
  }, [helpVisible]);

  const fetchSuggestions = async (searchQuery: string) => {
    try {
      const result = await invoke<string[]>('get_search_suggestions', {
        query: searchQuery,
        limit: 10
      });
      setSuggestions(result);
    } catch (e) {
      console.error('Failed to get suggestions:', e);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      e.preventDefault();
    } else if (e.key === 'Escape') {
      if (helpVisible) {
        setHelpVisible(false);
      } else if (query.length > 0) {
        setQuery('');
        onSearch('', filterType === 'files', filterType === 'directories');
      }
    }
  };

  const handleFilterChange = (value: string) => {
    setFilterType(value as 'files' | 'directories');
  };

  return (
    <div className="search-panel">
      <form onSubmit={(e) => e.preventDefault()} className="search-form">
        <div className="search-input-container">
          <input
            type="text"
            className="search-input"
            placeholder="搜索文件...ESC清空"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            autoFocus
            autoComplete="off"
          />
          {query.length > 0 && (
            <button
              type="button"
              className="clear-button"
              onClick={() => setQuery('')}
              title="清空搜索框"
            >
              ×
            </button>
          )}
        </div>
        <button
          type="button"
          className="help-button"
          ref={helpBtnRef}
          onClick={() => setHelpVisible((v) => !v)}
          title="搜索帮助"
        >
          ?
        </button>
        <select
          className="filter-select"
          value={filterType}
          onChange={(e) => handleFilterChange(e.target.value)}
        >
          <option value="files">仅文件</option>
          <option value="directories">仅文件夹</option>
        </select>
        {onExport && (
          <select
            className="export-select"
            title="导出..."
            value={exportFormat}
            onChange={(e) => {
              const format = e.target.value;
              if (format) {
                onExport(format as 'csv' | 'txt' | 'json');
                setExportFormat('');
              }
            }}
          >
            <option value="">导出...</option>
            <option value="csv">CSV</option>
            <option value="txt">TXT</option>
            <option value="json">JSON</option>
          </select>
        )}
        <button type="button"  className="settings-button" onClick={onOpenSettings} title="设置">
          ⚙
        </button>
      </form>
      {helpVisible && (
        <div className="help-popup" ref={helpRef}>
          <div className="help-popup-header">
            <span>搜索语法帮助</span>
            <button className="help-close" onClick={() => setHelpVisible(false)}>×</button>
          </div>
          <div className="help-popup-body">
            <div className="help-section">
              <h4>文件名搜索（Glob 通配符）</h4>
              <p>直接输入关键词搜索文件名，不区分大小写。支持通配符：</p>
              <div className="help-example"><code>*</code> 任意字符，<code>?</code> 单个字符，<code>[...]</code> 字符集</div>
              <div className="help-example"><code>chs*</code> 以 chs 开头</div>
              <div className="help-example"><code>*.rs</code> 所有 Rust 源文件</div>
              <div className="help-example"><code>pic?.jpg</code> 匹配 pic1.jpg、pica.jpg 等</div>
            </div>
            <div className="help-section">
              <h4>大小搜索</h4>
              <p><code>size:</code> 前缀 + 操作符 + 数值 + 单位</p>
              <div className="help-example"><code>{'size:>1GB'}</code> <code>{'size:<=500KB'}</code> <code>size:=10MB</code></div>
              <div className="help-example"><code>size:100MB</code> 无操作符时默认 &gt;=</div>
            </div>
            <div className="help-section">
              <h4>日期搜索</h4>
              <p>完整名：<code>datemodified:</code> <code>datecreated:</code> <code>dateaccessed:</code></p>
              <p>缩写：<code>dm:</code> <code>dc:</code> <code>da:</code></p>
              <p>格式：<code>YYYY/MM/DD</code> <code>YYYY-MM-DD</code> <code>YYYYMMDD</code> or <code>today</code></p>
              <div className="help-example"><code>dm:=2026/07/06</code> 修改日期等于某天</div>
              <div className="help-example"><code>{'dc:>=today'}</code> 今天及之后创建</div>
            </div>
            <div className="help-section">
              <h4>路径搜索</h4>
              <p><code>path:</code> 路径过滤，加 <code>:folder</code> 仅匹配文件夹</p>
              <div className="help-example"><code>path:Downloads</code> <code>path:C:\Users :folder</code></div>
            </div>
            <div className="help-section">
              <h4>正则搜索</h4>
              <div className="help-example"><code>regex:^\d{4}-.*\.txt$</code></div>
            </div>
            <div className="help-section">
              <h4>组合使用</h4>
              <p>空格分隔，全部条件需同时满足（AND 逻辑）。</p>
              <div className="help-example"><code>*.jpg size:&lt;500KB path:C:\Photos</code></div>
            </div>
          </div>
        </div>
      )}
      {suggestions.length > 0 && (
        <div className="suggestions">
          {suggestions.map((s, i) => (
            <div key={i} className="suggestion-item" onClick={() => { setQuery(s); onSearch(s); }}>
              {s}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export default SearchPanel;