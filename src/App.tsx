import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { save, message } from '@tauri-apps/plugin-dialog';
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
  const [searching, setSearching] = useState(false);
  const [searchTime, setSearchTime] = useState<number | null>(null);
  const [scrollTrigger, setScrollTrigger] = useState(0);
  const [rebuilding, setRebuilding] = useState(false);
  const initialLoadDone = useRef(false);

  useEffect(() => {
    if (initialLoadDone.current) return;
    initialLoadDone.current = true;
    loadIndexStatus();
    checkAdmin();
    loadAllFiles();
  }, []);

  useEffect(() => {
    const unlistenProgress = listen<{ volume: string; count: number }>('scan-progress', (_event) => {
      setStatusMessage(`扫描中: ${_event.payload.volume} (${_event.payload.count.toLocaleString()} 个文件)`);
    });

    const unlistenComplete = listen<{ volume: string; count: number }>('scan-complete', (_event) => {
      loadIndexStatus();
      setStatusMessage(`扫描完成: ${_event.payload.volume} (${_event.payload.count.toLocaleString()} 个文件)`);
      loadAllFiles();
    });

    const unlistenUpdated = listen<{ volume: string; count: number; cache_total?: number }>('index-updated', async (_event) => {
      // 删除事件（volume=""）跳过 loadIndexStatus，减少 ~30ms IPC 开销
      if (_event.payload.volume !== '') {
        loadIndexStatus();
      }
      if (_event.payload.cache_total !== undefined) {
        setTotalCount(_event.payload.cache_total);
      }
      // USN 增量更新会清除后端 search_cache，直接调用 fetchRecordsRange 会报 "cache expired"。
      // 改用 refreshCurrentView：先调用 search_files 重建缓存，再获取当前可见范围的数据。
      refreshCurrentView();
    });

    return () => {
      unlistenProgress.then(fn => fn());
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
    setStatusMessage('加载中...');
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
      if (response.total > 0) {
        setStatusMessage('');
      }
    } catch (e) {
      console.error('Failed to load all files:', e);
      message(`加载文件失败: ${e}`, { title: '错误', kind: 'error' });
    }
  };

  const sortStateRef = useRef(sortState);
  sortStateRef.current = sortState;

  // 跟踪当前搜索状态，供事件监听器闭包中访问最新值
  const searchStateRef = useRef(searchState);
  searchStateRef.current = searchState;

  const fetchCounterRef = useRef(0);
  const visibleRangeRef = useRef({ start: 0, end: 50 });
  const rangeCacheRef = useRef<Map<string, SearchResult[]>>(new Map());
  const rangeChangeTimerRef = useRef<number | null>(null);
  const PAGE_SIZE = 100;

  const fetchRecordsRange = useCallback(async (start: number, end: number) => {
    const cacheKey = `${start}-${end}`;
    const cached = rangeCacheRef.current.get(cacheKey);
    if (cached) {
      setResultsOffset(start);
      setResults(cached);
      return;
    }

    // 只读取当前 counter 值，不递增
    // 原因：递增会使 handleSortChange/handleSearch 的 myId 失效，
    // 导致用户主动排序/搜索的新结果被丢弃，旧数据残留
    const myId = fetchCounterRef.current;
    // 捕获 sortState 快照，await 期间若 sortState 变化（用户再次排序），
    // 则丢弃本次结果，防止旧排序数据覆盖新排序结果
    const sortSnapshot = { ...sortStateRef.current };
    const { field, direction } = sortSnapshot;
    try {
      const response = await invoke<RecordsRangeResponse>('get_records_range', { start, end, sortBy: field, sortDirection: direction });
      // 双重检查：counter 未变 且 sortState 未变，才应用结果
      if (myId === fetchCounterRef.current &&
        sortStateRef.current.field === sortSnapshot.field &&
        sortStateRef.current.direction === sortSnapshot.direction) {
        rangeCacheRef.current.set(cacheKey, response.results);
        // 缩容：从 20 条降至 10 条，每条 100 项
        // 10 × 100 × ~100字节 ≈ 100KB，足够覆盖滚动预取场景
        if (rangeCacheRef.current.size > 10) {
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
          .catch(() => { });
      }
    } catch (e) {
      console.error('Failed to fetch records range:', e);
      // 缓存过期是正常现象（USN 增量更新会清除缓存），不弹窗打扰用户
      const errMsg = String(e);
      if (errMsg.includes('Cache expired') || errMsg.includes('cache expired')) {
        // 静默处理，由 index-updated 事件的 refreshCurrentView 负责重建缓存
        return;
      }
      message(`获取数据失败: ${e}`, { title: '错误', kind: 'error' });
    }
  }, [totalCount]);

  /**
   * USN 增量更新后刷新当前视图
   *
   * 后端 apply_incremental_usn 会清除 search_cache，导致 get_records_range 返回 "cache expired" 错误。
   * 此函数通过重新调用 search_files 重建后端缓存，然后获取当前可见范围的数据，
   * 避免向用户显示错误弹窗，且不重置滚动位置。
   *
   * 所有外部状态通过 ref 访问，useCallback 依赖为空数组，确保事件监听器闭包捕获的版本始终有效。
   */
  const refreshCurrentView = useCallback(async () => {
    // 递增 fetchCounterRef，使正在进行的 fetchRecordsRange 失效，
    // 防止旧 fetchRecordsRange 返回的旧缓存数据覆盖 refreshCurrentView 的新结果
    ++fetchCounterRef.current;
    // 清除前端 range 缓存（后端缓存已被 USN 增量更新清除）
    rangeCacheRef.current.clear();
    const { field, direction } = sortStateRef.current;
    const { query, filesOnly, directoriesOnly } = searchStateRef.current;
    try {
      // 调用 search_files 重建后端缓存，直接使用返回的 first_batch，
      // 避免额外的 get_records_range IPC 调用（省 50-200ms）
      const response = await invoke<SearchResponse>('search_files', {
        params: { query, files_only: filesOnly, directories_only: directoriesOnly, sort_by: field, sort_direction: direction }
      });
      // search_files 已返回并重建后端缓存，再次递增 fetchCounterRef，
      // 使正在进行的旧 fetchRecordsRange 失效（它可能用了旧缓存），防止覆盖新结果
      ++fetchCounterRef.current;
      setTotalCount(response.total);
      if (response.total > 0) {
        setResultsOffset(0);
        setResults(response.results);
      } else {
        setResultsOffset(0);
        setResults([]);
      }
    } catch (e) {
      console.error('Failed to refresh current view after index update:', e);
    }
  }, []);

  const searchCounterRef = useRef(0);

  const handleSearch = useCallback(async (searchQuery: string, filesOnly?: boolean, directoriesOnly?: boolean) => {
    const myId = ++searchCounterRef.current;
    // 同步递增 fetchCounterRef，使正在进行的 fetchRecordsRange 失效，
    // 防止旧 fetchRecordsRange 返回的旧缓存数据覆盖 handleSearch 的新结果
    ++fetchCounterRef.current;
    rangeCacheRef.current.clear();
    setSearchState({ query: searchQuery, filesOnly: filesOnly ?? true, directoriesOnly: directoriesOnly ?? false });
    setScrollTrigger(prev => prev + 1);
    setSearching(true);
    setSearchTime(null);
    setStatusMessage('搜索中...');
    const startTime = performance.now();

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
      // search_files 已返回并重建后端缓存，再次递增 fetchCounterRef，
      // 使正在进行的旧 fetchRecordsRange 失效（它可能用了旧缓存），防止覆盖新结果
      ++fetchCounterRef.current;
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
    } finally {
      setSearchTime(performance.now() - startTime);
      setSearching(false);
    }
  }, [sortState]);

  const handleSortChange = useCallback(async (field: SortField, direction: SortDirection) => {
    // 清除 pending 的 fetchRecordsRange debounce，防止竞态：
    // resetScroll 会触发 onRangeChange，80ms 后调用 fetchRecordsRange，
    // 可能在 handleSortChange 完成前执行并覆盖结果
    if (rangeChangeTimerRef.current !== null) {
      clearTimeout(rangeChangeTimerRef.current);
      rangeChangeTimerRef.current = null;
    }
    const myId = ++fetchCounterRef.current;
    rangeCacheRef.current.clear();
    setSortState({ field, direction });
    setScrollTrigger(prev => prev + 1);
    setStatusMessage('排序中...');
    try {
      const searchResp = await invoke<SearchResponse>('search_files', {
        params: {
          query: searchState.query,
          files_only: searchState.filesOnly,
          directories_only: searchState.directoriesOnly,
          sort_by: field,
          sort_direction: direction
        }
      });
      if (myId !== fetchCounterRef.current) return;
      // search_files 已返回并重建后端缓存，递增 counter 使正在进行的
      // fetchRecordsRange 失效（它可能用了旧 sort state 的后端缓存），
      // 防止其返回的旧数据覆盖本次新排序结果
      ++fetchCounterRef.current;
      setTotalCount(searchResp.total);
      if (searchResp.total > 0) {
        // 直接用 search_files 返回的 first_batch（0-50），不再调用 get_records_range
        // 原因：减少一次 IPC 调用，且 search_files 已重建后端缓存，first_batch 就是新排序结果
        rangeCacheRef.current.set('0-50', searchResp.results);
        setResultsOffset(0);
        setResults(searchResp.results);
      } else {
        setResultsOffset(0);
        setResults([]);
      }
      setStatusMessage('');
    } catch (e) {
      console.error('Sort failed:', e);
      message(`排序失败: ${e}`, { title: '错误', kind: 'error' });
      setStatusMessage('');
    }
  }, [searchState]);

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
          searching={searching}
        />
        <ResultList
          results={results}
          resultsOffset={resultsOffset}
          totalCount={totalCount}
          sortField={sortState.field}
          sortDirection={sortState.direction}
          onOpenFile={handleOpenFile}
          onOpenFolder={handleOpenFolder}
          onVisibleRangeChange={handleVisibleRangeChange}
          onSortChange={handleSortChange}
          scrollToTop={scrollTrigger}
          searching={searching}
        />
      </div>
      <StatusBar
        message={statusMessage}
        indexStatus={indexStatus}
        isAdmin={isAdmin}
        searchTime={searchTime}
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
