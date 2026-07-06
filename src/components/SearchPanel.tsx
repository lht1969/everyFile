import { useState, useEffect, useRef } from 'react';

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
  const prevQueryRef = useRef(query);
  const prevFilterRef = useRef(filterType);

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

  const fetchSuggestions = async (searchQuery: string) => {
    try {
      const { invoke } = await import('@tauri-apps/api/core');
      const result = await invoke<string[]>('get_search_suggestions', {
        query: searchQuery,
        limit: 10
      });
      setSuggestions(result);
    } catch (e) {
      console.error('Failed to get suggestions:', e);
    }
  };

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      e.preventDefault();
    }
  };

  const handleFilterChange = (value: string) => {
    setFilterType(value as 'files' | 'directories');
  };

  return (
    <div className="search-panel">
      <form onSubmit={handleSubmit} className="search-form">
        <div className="search-input-container">
          <input
            type="text"
            className="search-input"
            placeholder="搜索文件... (支持 1.size:=<> 2.datemodified:/datecreated:/dateaccessed:=<> 3.path: 4.regex:)"
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