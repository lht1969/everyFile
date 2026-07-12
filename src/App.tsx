import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { save, ask, message } from '@tauri-apps/plugin-dialog';
import SearchPanel from './components/SearchPanel';
import ResultList from './components/ResultList';
import StatusBar from './components/StatusBar';
import SettingsModal from './components/SettingsModal';
import type { SearchResult, SearchResponse, RecordsRangeResponse, IndexStatus, SortField, SortDirection } from './types';
import './App.css';

function App() {
  const [results, setResults] = useState<SearchResult[]>([]);
  const [resultsOffset, setResultsOffset] = useState(0);
  const [totalCount, setTotalCount] = useState(0);
  const [statusMessage, setStatusMessage] = useState('');
  const [indexStatus, setIndexStatus] = useState<IndexStatus>({ status: 'ready', file_count: 0, progress: 1, message: '', volumes: [], last_update: '' });
  const [showSettings, setShowSettings] = useState(false);
  const [isAdmin, setIsAdmin] = useState(false);
  const [searchState, setSearchState] = useState({ query: '', filesOnly: true, directoriesOnly: false });
  const [sortState, setSortState] = useState<{ field: SortField; direction: SortDirection }>({ field: 'name', direction: 'asc' });
  const [scrollTrigger, setScrollTrigger] = useState(0);
  const [rebuilding, setRebuilding] = useState(false);

  useEffect(() => {
    loadIndexStatus();
    checkAdmin();
    loadAllFiles();
  }, []);

  useEffect(() => {
    const unlistenComplete = listen<{ volume: string; count: number }>('scan-complete', (_event) => {
      loadIndexStatus();
    });

    const unlistenUpdated = listen<{ volume: string; count: number; cache_total?: number }>('index-updated', async (_event) => {
      loadIndexStatus();
      if (_event.payload.cache_total !== undefined) {
        setTotalCount(_event.payload.cache_total);
      }
      const { start, end } = visibleRangeRef.current;
      fetchRecordsRange(start, end);
    });

    return () => {
      unlistenComplete.then(fn => fn());
      unlistenUpdated.then(fn => fn());
    };
  }, []);

  useEffect(() => {
    const interval = setInterval(() => {
      if (indexStatus.status === 'scanning') {
        loadIndexStatus();
      }
    }, 5000);
    return () => clearInterval(interval);
  }, [indexStatus.status]);

  const checkAdmin = async () => {
    try {
      const admin = await invoke<boolean>('is_admin');
      setIsAdmin(admin);
    } catch (e) {
      console.error('Failed to check admin:', e);
    }
  };

  const loadIndexStatus = async () => {
    try {
      const status = await invoke<IndexStatus>('get_index_status');
      setIndexStatus(status);
    } catch (e) {
      console.error('Failed to load index status:', e);
    }
  };

  const loadAllFiles = async () => {
    try {
      const response = await invoke<SearchResponse>('search_files', {
        params: { query: '', files_only: true, sort_by: sortState.field, sort_direction: sortState.direction }
      });
      setTotalCount(response.total);
      if (response.results.length > 0) {
        setResultsOffset(0);
        setResults(response.results);
      } else if (response.total > 0) {
        const range = await invoke<RecordsRangeResponse>('get_records_range', { start: 0, end: 50, sortBy: sortState.field, sortDirection: sortState.direction });
        setResultsOffset(0);
        setResults(range.results);
      }
    } catch (e) {
      console.error('Failed to load all files:', e);
      message(`加载文件失败: ${e}`, { title: '错误', kind: 'error' });
    }
  };

  const sortStateRef = useRef(sortState);
  sortStateRef.current = sortState;

  const fetchCounterRef = useRef(0);
  const visibleRangeRef = useRef({ start: 0, end: 50 });
  const rangeCacheRef = useRef<Map<string, SearchResult[]>>(new Map());
  const PAGE_SIZE = 100;

  const fetchRecordsRange = useCallback(async (start: number, end: number) => {
    const cacheKey = `${start}-${end}`;
    const cached = rangeCacheRef.current.get(cacheKey);
    if (cached) {
      setResultsOffset(start);
      setResults(cached);
      return;
    }

    const myId = ++fetchCounterRef.current;
    const { field, direction } = sortStateRef.current;
    try {
      const response = await invoke<RecordsRangeResponse>('get_records_range', { start, end, sortBy: field, sortDirection: direction });
      if (myId === fetchCounterRef.current) {
        rangeCacheRef.current.set(cacheKey, response.results);
        if (rangeCacheRef.current.size > 20) {
          const firstKey = rangeCacheRef.current.keys().next().value;
          if (firstKey) rangeCacheRef.current.delete(firstKey);
        }
        setResultsOffset(start);
        setResults(response.results);
      }

      const nextStart = end;
      const nextEnd = nextStart + PAGE_SIZE;
      const nextKey = `${nextStart}-${nextEnd}`;
      if (!rangeCacheRef.current.has(nextKey) && nextStart < (totalCount || 0)) {
        invoke<RecordsRangeResponse>('get_records_range', { start: nextStart, end: nextEnd, sortBy: field, sortDirection: direction })
          .then(resp => { rangeCacheRef.current.set(nextKey, resp.results); })
          .catch(() => {});
      }
    } catch (e) {
      console.error('Failed to fetch records range:', e);
      message(`获取数据失败: ${e}`, { title: '错误', kind: 'error' });
    }
  }, [totalCount]);

  const searchCounterRef = useRef(0);

  const handleSearch = useCallback(async (searchQuery: string, filesOnly?: boolean, directoriesOnly?: boolean) => {
    const myId = ++searchCounterRef.current;
    rangeCacheRef.current.clear();
    setSearchState({ query: searchQuery, filesOnly: filesOnly ?? true, directoriesOnly: directoriesOnly ?? false });
    setScrollTrigger(prev => prev + 1);
    setStatusMessage('搜索中...');

    try {
      const response = await invoke<SearchResponse>('search_files', {
        params: {
          query: searchQuery,
          files_only: filesOnly,
          directories_only: directoriesOnly,
          sort_by: sortState.field,
          sort_direction: sortState.direction
        }
      });
      if (myId !== searchCounterRef.current) return;
      setTotalCount(response.total);
      if (response.results.length > 0) {
        setResultsOffset(0);
        setResults(response.results);
      } else if (response.total > 0) {
        const range = await invoke<RecordsRangeResponse>('get_records_range', { start: 0, end: 50, sortBy: sortState.field, sortDirection: sortState.direction });
        if (myId === searchCounterRef.current) {
          setResultsOffset(0);
          setResults(range.results);
        }
      } else {
        setResultsOffset(0);
        setResults([]);
      }
      setStatusMessage(searchQuery.trim() ? `找到 ${response.total} 个结果` : '');
    } catch (e) {
      if (myId !== searchCounterRef.current) return;
      console.error('Search failed:', e);
      message(`搜索失败: ${e}`, { title: '错误', kind: 'error' });
    }
  }, [sortState]);

  const handleSortChange = useCallback(async (field: SortField, direction: SortDirection) => {
    const myId = ++fetchCounterRef.current;
    rangeCacheRef.current.clear();
    setSortState({ field, direction });
    setScrollTrigger(prev => prev + 1);
    try {
      const response = await invoke<RecordsRangeResponse>('get_sorted_range', {
        sortBy: field,
        sortDirection: direction,
        start: 0,
        end: 50
      });
      if (myId === fetchCounterRef.current) {
        setTotalCount(response.total);
        setResultsOffset(0);
        setResults(response.results);
      }
    } catch (e) {
      console.error('Sort failed, falling back to re-search:', e);
      try {
        await invoke<SearchResponse>('search_files', {
          params: {
            query: searchState.query,
            files_only: searchState.filesOnly,
            directories_only: searchState.directoriesOnly,
            sort_by: field,
            sort_direction: direction
          }
        });
        const response = await invoke<RecordsRangeResponse>('get_sorted_range', {
          sortBy: field,
          sortDirection: direction,
          start: 0,
          end: 50
        });
        if (myId === fetchCounterRef.current) {
          setTotalCount(response.total);
          setResultsOffset(0);
          setResults(response.results);
        }
      } catch (e2) {
        console.error('Fallback sort also failed:', e2);
        message(`排序失败: ${e2}`, { title: '错误', kind: 'error' });
      }
    }
  }, [searchState]);

  const rangeChangeTimerRef = useRef<number | null>(null);

  const handleVisibleRangeChange = useCallback((start: number, end: number) => {
    visibleRangeRef.current = { start, end };
    if (rangeChangeTimerRef.current !== null) {
      clearTimeout(rangeChangeTimerRef.current);
    }
    rangeChangeTimerRef.current = window.setTimeout(() => {
      fetchRecordsRange(start, end);
    }, 80);
  }, [fetchRecordsRange]);

  const handleOpenFile = async (path: string) => {
    try {
      await invoke('open_file', { path });
    } catch (e) {
      console.error('Failed to open file:', e);
      message(`无法打开文件: ${e}`, { title: '错误', kind: 'error' });
    }
  };

  const handleOpenFolder = async (path: string) => {
    try {
      await invoke('open_folder', { path });
    } catch (e) {
      console.error('Failed to open folder:', e);
      message(`无法打开文件夹: ${e}`, { title: '错误', kind: 'error' });
    }
  };

  const handleDeleteFile = async (path: string) => {
    try {
      const confirmed = await ask(`确定要删除 "${path}" 吗？`, { title: '确认删除', kind: 'warning' });
      if (!confirmed) return;
      await invoke('delete_file', { path });
      handleSearch(searchState.query, searchState.filesOnly, searchState.directoriesOnly);
    } catch (e) {
      console.error('Failed to delete file:', e);
      message(`删除失败: ${e}`, { title: '错误', kind: 'error' });
    }
  };

  const handleRebuildIndex = async () => {
    setRebuilding(true);
    try {
      await invoke('rebuild_index');
      await loadIndexStatus();
      loadAllFiles();
    } catch (e) {
      console.error('Failed to rebuild index:', e);
      message(`重建索引失败: ${e}`, { title: '错误', kind: 'error' });
    } finally {
      setRebuilding(false);
    }
  };

  const handleExport = async (format: 'csv' | 'txt' | 'json') => {
    const beijingTime = new Date(new Date().getTime() + 8 * 60 * 60 * 1000);
    const timestamp = beijingTime.toISOString().replace(/[:.]/g, '-');
    const filename = `everything_export_${timestamp}`;
    const ext = format === 'csv' ? 'csv' : format === 'txt' ? 'txt' : 'json';

    try {
      const path = await save({
        defaultPath: `${filename}.${ext}`,
        filters: [{
          name: format.toUpperCase(),
          extensions: [ext]
        }]
      });

      if (!path) {
        return;
      }

      await invoke('export_all_results', {
        query: searchState.query,
        filesOnly: searchState.filesOnly,
        directoriesOnly: searchState.directoriesOnly,
        format,
        path
      });
    } catch (e) {
      console.error('Export failed:', e);
      message(`导出失败: ${e}`, { title: '错误', kind: 'error' });
    }
  };

  return (
    <div className="app">
      <div className="main-content">
        <SearchPanel
          onSearch={handleSearch}
          onOpenSettings={() => setShowSettings(true)}
          onExport={handleExport}
        />
        <ResultList
          results={results}
          resultsOffset={resultsOffset}
          totalCount={totalCount}
          sortField={sortState.field}
          sortDirection={sortState.direction}
          onOpenFile={handleOpenFile}
          onOpenFolder={handleOpenFolder}
          onDeleteFile={handleDeleteFile}
          onVisibleRangeChange={handleVisibleRangeChange}
          onSortChange={handleSortChange}
          scrollToTop={scrollTrigger}
        />
      </div>
      <StatusBar
        message={statusMessage}
        indexStatus={indexStatus}
        isAdmin={isAdmin}
      />

      {showSettings && (
        <SettingsModal
          onClose={() => setShowSettings(false)}
          onRebuildIndex={handleRebuildIndex}
          indexStatus={indexStatus}
          onVolumeChange={() => loadAllFiles()}
          rebuilding={rebuilding}
        />
      )}
    </div>
  );
}

export default App;
