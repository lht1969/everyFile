import { useState, useEffect } from 'react';

interface SearchPanelProps {
  onSearch: (query: string, filesOnly?: boolean, directoriesOnly?: boolean) => void;
  onOpenSettings: () => void;
  isAdmin: boolean;
}

function SearchPanel({ onSearch, onOpenSettings, isAdmin }: SearchPanelProps) {
  const [query, setQuery] = useState('');
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const [filesOnly, setFilesOnly] = useState(true);
  const [directoriesOnly, setDirectoriesOnly] = useState(false);

  useEffect(() => {
    const debounce = setTimeout(() => {
      if (query.trim()) {
        fetchSuggestions(query);
      } else {
        setSuggestions([]);
      }
    }, 300);
    return () => clearTimeout(debounce);
  }, [query]);

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
    onSearch(query, filesOnly, directoriesOnly);
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      onSearch(query, filesOnly, directoriesOnly);
    }
  };

  const handleFilesOnlyChange = (checked: boolean) => {
    setFilesOnly(checked);
    if (checked) setDirectoriesOnly(false);
    onSearch(query, checked, false);
  };

  const handleDirectoriesOnlyChange = (checked: boolean) => {
    setDirectoriesOnly(checked);
    if (checked) setFilesOnly(false);
    onSearch(query, false, checked);
  };

  return (
    <div className="search-panel">
      <form onSubmit={handleSubmit} className="search-form">
        <div className="search-input-container">
          <input
            type="text"
            className="search-input"
            placeholder="搜索文件... (支持 size: datemodified: path: regex:)"
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
        <label className="filter-checkbox">
          <input
            type="checkbox"
            checked={filesOnly}
            onChange={(e) => handleFilesOnlyChange(e.target.checked)}
          />
          仅文件
        </label>
        <label className="filter-checkbox">
          <input
            type="checkbox"
            checked={directoriesOnly}
            onChange={(e) => handleDirectoriesOnlyChange(e.target.checked)}
          />
          仅目录
        </label>
        <button type="submit" className="search-button">
          搜索
        </button>
        <button type="button" className="settings-button" onClick={onOpenSettings} title="设置">
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
      {!isAdmin && (
        <div className="admin-warning">
          ⚠ 非管理员模式，部分功能可能受限
        </div>
      )}
    </div>
  );
}

export default SearchPanel;