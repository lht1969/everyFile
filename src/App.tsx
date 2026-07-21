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
    });

    // 增量更新后刷新当前可见范围（文件删除/修改/新增时立即更新显示）
    // 仅清除覆盖当前可见范围的缓存条目，保留其他范围的缓存避免不必要的重新拉取
    // 关键优化：检查变更文件是否在当前可见范围内，如果不在则跳过 fetch，
    // 避免 200 万文件中极少数变更触发不必要的排序和数据传输。
    const unlistenRefresh = listen<{ added?: number; updated?: number; removed?: number; total?: number; changed_fids?: number[] }>('records-refresh', async (event) => {
      const added = event.payload.added ?? 0;
      const updated = event.payload.updated ?? 0;
      const removed = event.payload.removed ?? 0;
      // 无实质变化时不刷新，避免空轮询导致的前端开销
      if (added === 0 && updated === 0 && removed === 0) {
        return;
      }

      // 同步更新总数，让滚动条高度反映最新数据量
      if (typeof event.payload.total === 'number') {
        setTotalCount(event.payload.total);
      }

      // 拖动滑块时暂不出 delta 信息，记录 pending 待停止后刷新
      if (isDraggingRef.current) {
        pendingRefreshRef.current = true;
        return;
      }

      // 关键优化：检查变更文件是否在当前可见结果中
      // 200万文件中只有20个可见，变更文件命中的概率极低（~0.001%），
      // 大多数增量更新不需要刷新窗口。
      const changed_fids = event.payload.changed_fids;
      if (changed_fids && changed_fids.length > 0 && results.length > 0) {
        const visibleFids = new Set(results.map(r => r.file_id));
        const hasVisibleChange = changed_fids.some(fid => visibleFids.has(fid));
        if (!hasVisibleChange) {
          // 变更文件不在可见范围内，跳过 fetch，仅更新总数
          return;
        }
      }

      // 清除覆盖当前可见范围的缓存，并主动触发一次 fetch，让删除/修改/新增
      // 在静止状态下也能立即反映到窗口。拖动期间不主动 fetch，避免与用户
      // 停止滚动后的 fetch 竞争导致乱序。
      const { start, end } = visibleRangeRef.current;
      if (start !== undefined && end !== undefined && end > start) {
        const fetchStart = Math.max(0, start - 50);
        const fetchEnd = start + FETCH_SIZE;
        for (const key of rangeCacheRef.current.keys()) {
          const [s, e] = key.split('-').map(Number);
          if (fetchStart >= s && fetchEnd <= e) {
            rangeCacheRef.current.delete(key);
            break;
          }
        }
        // 使正在进行的 fetchRecordsRange 失效，防止旧数据覆盖刷新结果
        ++fetchCounterRef.current;
        isFetchingRef.current = false;
        // 立即刷新当前可见范围，确保文件被删除/重命名后窗口自动更新
        await fetchRecordsRangeRef.current(start, 0);
      }
    });

    return () => {
      unlistenProgress.then(fn => fn());
      unlistenComplete.then(fn => fn());
      unlistenUpdated.then(fn => fn());
      unlistenRefresh.then(fn => fn());
    };
  }, []);

  useEffect(() => {
    // 扫描中需要及时反馈进度，保持 5 秒；空闲时只需偶尔同步状态，60 秒足够
    const intervalMs = indexStatus.status === 'scanning' ? 5000 : 60000;
    const interval = setInterval(() => {
      loadIndexStatus();
    }, intervalMs);
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
      if (response.total > 0) {
        // Fetch a larger initial range to cover scroll area
        try {
          const range = await invoke<RecordsRangeResponse>('get_records_range', { start: 0, end: 500, sortBy: sortState.field, sortDirection: sortState.direction });
          rangeCacheRef.current.set(`0-${range.results.length}`, range.results);
          setResultsOffset(0);
          setResults(range.results);
        } catch {
          // Fallback to first_batch from search_files
          setResultsOffset(0);
          setResults(response.results);
        }
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
  // 拖动滑块状态：true 表示用户正在拖动滚动条，此时暂停 records-refresh 刷新
  const isDraggingRef = useRef(false);
  // 拖动期间有 pending 的 records-refresh，停止拖动后刷新前后 50 行
  const pendingRefreshRef = useRef(false);
  const FETCH_SIZE = 200;
  const isFetchingRef = useRef(false);

  const fetchRecordsRange = useCallback(async (start: number, _end: number) => {
    if (isFetchingRef.current) return;
    const fetchStart = Math.max(0, start - 50);
    const fetchEnd = start + FETCH_SIZE;
    const cacheKey = `${fetchStart}-${fetchEnd}`;

    // Check cache
    const existingKey = Array.from(rangeCacheRef.current.keys()).find(k => {
      const [s, e] = k.split('-').map(Number);
      return fetchStart >= s && fetchEnd <= e;
    });
    if (existingKey) {
      const cached = rangeCacheRef.current.get(existingKey)!;
      const offset = parseInt(existingKey.split('-')[0]);
      setResultsOffset(offset);
      setResults(cached);
      return;
    }

    isFetchingRef.current = true;
    const myId = fetchCounterRef.current;
    const sortSnapshot = { ...sortStateRef.current };
    const { field, direction } = sortSnapshot;
    const reqStart = performance.now();
    try {
      const response = await invoke<RecordsRangeResponse>('get_records_range', { start: fetchStart, end: fetchEnd, sortBy: field, sortDirection: direction });
      if (myId === fetchCounterRef.current &&
        sortStateRef.current.field === sortSnapshot.field &&
        sortStateRef.current.direction === sortSnapshot.direction) {
        rangeCacheRef.current.set(cacheKey, response.results);
        if (rangeCacheRef.current.size > 3) {
          const firstKey = rangeCacheRef.current.keys().next().value;
          if (firstKey) rangeCacheRef.current.delete(firstKey);
        }
        setResultsOffset(fetchStart);
        setResults(response.results);
        if (response.total !== totalCount) {
          setTotalCount(response.total);
        }
        console.log('[FETCH] start=', start, 'ms=', (performance.now() - reqStart).toFixed(0), 'first=', response.results[0]?.name, 'last=', response.results[response.results.length - 1]?.name);
      }
    } catch (e) {
      console.error('Failed to fetch records range:', e);
      const errMsg = String(e);
      if (errMsg.includes('Cache expired') || errMsg.includes('cache expired')) {
        return;
      }
      message(`获取数据失败: ${e}`, { title: '错误', kind: 'error' });
    } finally {
      isFetchingRef.current = false;
    }
  }, [totalCount]);

  const searchCounterRef = useRef(0);
  // 排序操作专用计数器：仅 handleSearch 和新的 handleSortChange 会递增，
  // records-refresh 事件不递增此计数器，避免 USN 增量更新打断用户主动排序
  const sortCounterRef = useRef(0);
  const fetchRecordsRangeRef = useRef(fetchRecordsRange);
  fetchRecordsRangeRef.current = fetchRecordsRange;

  const handleSearch = useCallback(async (searchQuery: string, filesOnly?: boolean, directoriesOnly?: boolean) => {
    const myId = ++searchCounterRef.current;
    // 同步递增 fetchCounterRef，使正在进行的 fetchRecordsRange 失效，
    // 防止旧 fetchRecordsRange 返回的旧缓存数据覆盖 handleSearch 的新结果
    ++fetchCounterRef.current;
    // 同步递增 sortCounterRef，使正在进行的 handleSortChange 失效，
    // 防止旧 handleSortChange 返回的结果覆盖 handleSearch 的新结果
    ++sortCounterRef.current;
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
      ++fetchCounterRef.current;
      // search_files 完成后再次递增 sortCounterRef，使 search 期间触发的 handleSortChange 失效
      ++sortCounterRef.current;
      setTotalCount(response.total);
      if (response.total > 0) {
        // Fetch a larger initial range to cover scroll area
        try {
          const range = await invoke<RecordsRangeResponse>('get_records_range', { start: 0, end: 500, sortBy: sortState.field, sortDirection: sortState.direction });
          if (myId === searchCounterRef.current) {
            rangeCacheRef.current.set('0-500', range.results);
            setResultsOffset(0);
            setResults(range.results);
          }
        } catch {
          if (myId === searchCounterRef.current) {
            setResultsOffset(0);
            setResults(response.results);
          }
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
    if (rangeChangeTimerRef.current !== null) {
      clearTimeout(rangeChangeTimerRef.current);
      rangeChangeTimerRef.current = null;
    }
    // 使用 sortCounterRef 而非 fetchCounterRef：
    // records-refresh 事件只递增 fetchCounterRef，不会取消用户主动排序
    // 仅 handleSearch 和新的 handleSortChange 会递增 sortCounterRef
    const myId = ++sortCounterRef.current;
    rangeCacheRef.current.clear();
    setSortState({ field, direction });
    setScrollTrigger(prev => prev + 1);
    setStatusMessage('排序中...');
    try {
      let range: RecordsRangeResponse;
      try {
        range = await invoke<RecordsRangeResponse>('get_records_range', {
          start: 0, end: 500, sortBy: field, sortDirection: direction
        });
      } catch {
        const searchResp = await invoke<SearchResponse>('search_files', {
          params: {
            query: searchState.query,
            files_only: searchState.filesOnly,
            directories_only: searchState.directoriesOnly,
            sort_by: field,
            sort_direction: direction
          }
        });
        // 被取消时必须清除状态消息，避免"排序中"永久显示
        if (myId !== sortCounterRef.current) {
          setStatusMessage('');
          return;
        }
        setTotalCount(searchResp.total);
        if (searchResp.total === 0) {
          setResultsOffset(0);
          setResults([]);
          setStatusMessage('');
          return;
        }
        range = await invoke<RecordsRangeResponse>('get_records_range', {
          start: 0, end: 500, sortBy: field, sortDirection: direction
        });
      }
      // 被取消时必须清除状态消息，避免"排序中"永久显示
      if (myId !== sortCounterRef.current) {
        setStatusMessage('');
        return;
      }
      setTotalCount(range.total);
      rangeCacheRef.current.set(`0-${range.results.length}`, range.results);
      setResultsOffset(0);
      setResults(range.results);
      setStatusMessage('');
    } catch (e) {
      console.error('Sort failed:', e);
      message(`排序失败: ${e}`, { title: '错误', kind: 'error' });
      setStatusMessage('');
    }
  }, [searchState]);

  const handleVisibleRangeChange = useCallback((start: number, end: number) => {
    visibleRangeRef.current = { start, end };
    // 拖动期间标记状态，暂停 records-refresh 刷新
    isDraggingRef.current = true;
    if (rangeChangeTimerRef.current !== null) {
      clearTimeout(rangeChangeTimerRef.current);
    }
    rangeChangeTimerRef.current = window.setTimeout(async () => {
      // 拖动停止：只执行一次 fetch，等数据应用后再清 isDraggingRef。
      // 不再主动补 refresh fetch，避免同一位置两次 fetch 因 delta 变化返回不同结果导致乱序。
      // refresh 事件在拖动期间被暂存，此处 pending 仅用于清除缓存，让下一次正常刷新获取最新数据。
      if (pendingRefreshRef.current) {
        pendingRefreshRef.current = false;
        const fetchStart = Math.max(0, start - 50);
        const fetchEnd = start + FETCH_SIZE;
        for (const key of rangeCacheRef.current.keys()) {
          const [s, e] = key.split('-').map(Number);
          if (fetchStart >= s && fetchEnd <= e) {
            rangeCacheRef.current.delete(key);
            break;
          }
        }
        ++fetchCounterRef.current;
        isFetchingRef.current = false;
      }
      await fetchRecordsRangeRef.current(start, end);
      // fetch 完成（数据已应用到 UI）后才允许 refresh 事件立即处理
      isDraggingRef.current = false;
    }, 100);
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
