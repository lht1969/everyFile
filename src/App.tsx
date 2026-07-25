import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
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
  const [indexStatus, setIndexStatus] = useState<IndexStatus>({ status: 'ready', file_count: 0, progress: 1, message: '', volumes: [], last_update: '', scanning_volumes: [] });
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

  // 全局 ESC 键关闭窗口（焦点不在输入框时）
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
      try { getCurrentWindow().close(); } catch (err) { console.error('[ESC] close failed:', err); }
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, []);

  useEffect(() => {
    const unlistenProgress = listen<{ volume: string }>('scan-progress', (_event) => {
      setStatusMessage(`${_event.payload.volume} 加载中...`);
    });

    const unlistenComplete = listen<{ volume: string; count: number }>('scan-complete', (_event) => {
      setStatusMessage(`扫描完成: ${_event.payload.volume} (${_event.payload.count.toLocaleString()} 个文件)`);
    });

    const unlistenAllComplete = listen('scan-all-complete', () => {
      loadAllFiles();
      loadIndexStatus();
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
      console.log('[REFRESH] received added=' + added + ' updated=' + updated + ' removed=' + removed + ' isFetching=' + isFetchingRef.current + ' resultsLen=' + resultsRef.current.length);
      // 无实质变化时不刷新，避免空轮询导致的前端开销
      if (added === 0 && updated === 0 && removed === 0) {
        return;
      }

      const changed_fids = event.payload.changed_fids;

      // 先检查变更文件是否在当前可见结果中（必须在 setTotalCount 之前）。
      // 200万文件中只有几十个可见，变更文件命中的概率极低，
      // 大多数增量更新不需要刷新窗口。
      let hasVisibleChange = false;
      if (changed_fids && changed_fids.length > 0 && resultsRef.current.length > 0) {
        const visibleFids = new Set(resultsRef.current.map(r => r.file_id));
        hasVisibleChange = changed_fids.some(fid => visibleFids.has(fid));
      }

      // 当用户滚动到底部附近时，仅对不可见变更跳过 fetch。
      // 可见文件的删除/修改/新增必须触发 fetch，否则用户看到的文件不会更新。
      // 原因：后端 USN 日志处理几乎每次事件都报告 added > 0（索引微调），
      // 原 !hasAdded 条件使底部保护从未生效，改为基于可见性判断。
      // 注意：当 changed_fids 未提供时（如右键菜单删除/剪切），
      // 必须触发 fetch 以确保删除的文件从显示结果中移除。
      const currentEnd = visibleRangeRef.current.end ?? 0;
      const isNearBottom = currentEnd > 0 && currentEnd >= totalCountRef.current - 1 - 5
        && totalCountRef.current > FETCH_SIZE;
      if (isNearBottom && changed_fids && !hasVisibleChange) {
        console.log('[REFRESH] near bottom, skip fetch (no visible change), update total=' + event.payload.total);
        if (typeof event.payload.total === 'number') {
          setTotalCount(event.payload.total);
        }
        // 标记 pending，确保用户滚动离开底部时清除缓存并 fetch 最新数据
        pendingRefreshRef.current = true;
        return;
      }

      // 拖动滑块时暂不出 delta 信息，记录 pending 待停止后刷新
      if (isDraggingRef.current) {
        pendingRefreshRef.current = true;
        return;
      }

      // 注意：不在此处调用 setTotalCount。
      // 原因：setTotalCount 会触发 useVirtualScroll 重新计算 startIndex/endIndex，
      // 若 endIndex 改变（如 totalItems 从 8 减至 7），则 onRangeChange 被触发，
      // handleVisibleRangeChange 在 atBottom=true 时执行 ++fetchCounterRef，
      // 使下方 fetchRecordsRange 的 myId 失效，导致结果被丢弃，形成空白窗口。
      // totalCount 由 fetchRecordsRange 完成后内部 setTotalCount 自然更新。
      // 状态栏"找到 X 个结果"也在 fetchRecordsRange 完成后更新。

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
        // 仅在没有正在进行的 fetch 时才主动刷新。
        // 不递增 fetchCounterRef，避免使正在进行的 fetchRecordsRange 失效——
        // 否则旧 fetch 的结果被丢弃，而新 fetch 尚未返回，期间 results
        // 可能不覆盖当前可见范围，导致 4 秒空白窗口。
        // 如果有正在进行的 fetch，标记 pending，在 fetch 完成后自动刷新，
        // 确保删除/新增的文件能及时反映到窗口。
        if (!isFetchingRef.current) {
          console.log('[REFRESH] triggering fetch start=' + start);
          await fetchRecordsRangeRef.current(start, 0);
        } else {
          console.log('[REFRESH] setting pending (isFetching=true)');
          pendingRefreshAfterFetchRef.current = true;
        }
      }
    });

    // 右键菜单操作后触发刷新（删除、剪切+粘贴、重命名等）
    // 独立于 records-refresh，不受空变化过滤拦截
    const unlistenRefreshVisible = listen('refresh-visible', async () => {
      console.log('[REFRESH-VISIBLE] received, triggering fetch');
      const { start } = visibleRangeRef.current;
      if (start !== undefined && start >= 0) {
        if (isFetchingRef.current) {
          pendingRefreshAfterFetchRef.current = true;
        } else {
          // 清除覆盖当前可见范围的缓存，确保获取最新数据
          const fetchStart = Math.max(0, start - 50);
          const fetchEnd = start + FETCH_SIZE;
          for (const key of rangeCacheRef.current.keys()) {
            const [s, e] = key.split('-').map(Number);
            if (fetchStart >= s && fetchEnd <= e) {
              rangeCacheRef.current.delete(key);
              break;
            }
          }
          await fetchRecordsRangeRef.current(start, 0);
        }
      }
    });

    return () => {
      unlistenProgress.then(fn => fn());
      unlistenComplete.then(fn => fn());
      unlistenAllComplete.then(fn => fn());
      unlistenUpdated.then(fn => fn());
      unlistenRefresh.then(fn => fn());
      unlistenRefreshVisible.then(fn => fn());
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
      // 全量扫描阶段：后端返回 scanning_volumes，前端据此显示加载状态
      // （scan-progress 事件可能在前端 listener 注册前已发出，需要此兜底）
      if (status.scanning_volumes.length > 0) {
        setStatusMessage(`${status.scanning_volumes[0]} 加载中...`);
      } else if (statusMessage.endsWith('加载中...')) {
        // 扫描结束，清除加载状态消息
        setStatusMessage('');
      }
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

  const resultsRef = useRef(results);
  resultsRef.current = results;

  const fetchCounterRef = useRef(0);
  const totalCountRef = useRef(0);
  totalCountRef.current = totalCount;
  const visibleRangeRef = useRef({ start: 0, end: 50 });
  const rangeCacheRef = useRef<Map<string, SearchResult[]>>(new Map());
  const rangeChangeTimerRef = useRef<number | null>(null);
  // 拖动滑块状态：true 表示用户正在拖动滚动条，此时暂停 records-refresh 刷新
  const isDraggingRef = useRef(false);
  // 拖动期间有 pending 的 records-refresh，停止拖动后刷新前后 50 行
  const pendingRefreshRef = useRef(false);
  const FETCH_SIZE = 200;
  const isFetchingRef = useRef(false);
  // records-refresh 期间如果有进行中的 fetch，标记 pending，
  // 在 fetch 完成后自动刷新当前可见范围，确保增量更新及时反映
  const pendingRefreshAfterFetchRef = useRef(false);

  const fetchRecordsRange = useCallback(async (start: number, _end: number) => {
    if (isFetchingRef.current) {
      console.log('[FETCH] skipped: isFetching=true start=' + start);
      return;
    }
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
      console.log('[FETCH] cache hit start=' + start + ' offset=' + offset + ' len=' + cached.length);
      setResultsOffset(offset);
      setResults(cached);
      return;
    }

    isFetchingRef.current = true;
    const myId = fetchCounterRef.current;
    const sortSnapshot = { ...sortStateRef.current };
    const { field, direction } = sortSnapshot;
    const reqStart = performance.now();
    console.log('[FETCH] begin start=' + start + ' fetchStart=' + fetchStart + ' fetchEnd=' + fetchEnd + ' myId=' + myId);
    try {
      const response = await invoke<RecordsRangeResponse>('get_records_range', { start: fetchStart, end: fetchEnd, sortBy: field, sortDirection: direction });
      const elapsed = (performance.now() - reqStart).toFixed(0);
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
        if (response.total !== totalCountRef.current) {
          setTotalCount(response.total);
        }
        // 更新状态栏"找到 X 个结果"，确保删除/创建文件后状态栏数量同步更新。
        // 注意：必须基于 response.total（后端最新值），不能用 totalCountRef.current，
        // 因为 setTotalCount 是异步的，此时 totalCountRef.current 可能还是旧值。
        const curQuery = searchStateRef.current.query;
        if (curQuery.trim()) {
          setStatusMessage(`找到 ${response.total} 个结果`);
        }
        console.log('[FETCH] done start=' + start + ' ms=' + elapsed + ' total=' + response.total + ' resultsLen=' + response.results.length + ' first=' + response.results[0]?.name + ' last=' + response.results[response.results.length - 1]?.name);
      } else {
        console.log('[FETCH] discarded start=' + start + ' ms=' + elapsed + ' myId=' + myId + ' curId=' + fetchCounterRef.current);
      }
    } catch (e) {
      console.error('Failed to fetch records range:', e);
      const errMsg = String(e);
      if (errMsg.includes('Cache expired') || errMsg.includes('cache expired')) {
        console.log('[FETCH] cache expired, skipping start=' + start);
        return;
      }
      message(`获取数据失败: ${e}`, { title: '错误', kind: 'error' });
    } finally {
      isFetchingRef.current = false;
      // 检查是否有 pending 的 refresh（records-refresh 期间有进行中 fetch 时设置）
      // 如果有，在当前 fetch 完成后自动刷新当前可见范围，确保增量更新及时反映。
      // 注意：必须先清除标志再刷新，避免刷新过程中又设置 pending 导致无限循环。
      if (pendingRefreshAfterFetchRef.current) {
        pendingRefreshAfterFetchRef.current = false;
        // 底部时跳过 pending refresh：
        // 此次 fetch 已通过 setResults 更新了可见数据，setTotalCount 的影响
        // 被 useVirtualScroll 的 atBottom+totalItemsChanged 守卫拦截，不会触发新 fetch。
        // 若此处再执行 pending fetch，会再次 setTotalCount → 虽被拦截但浪费 CPU。
        // 下一次 records-refresh 事件会自然触发新的 fetch。
        const isAtBottom = totalCountRef.current > 0 &&
          (visibleRangeRef.current.end ?? 0) >= totalCountRef.current - 1 - 5
          && totalCountRef.current > FETCH_SIZE;
        if (!isAtBottom) {
          const curStart = visibleRangeRef.current.start;
          if (curStart !== undefined && curStart >= 0) {
            // 删除覆盖当前可见范围的 cache，确保不会 cache hit 返回旧数据
            // （当前 fetch 刚完成可能设置了新 cache，但该 cache 可能不含增量更新）
            const fs = Math.max(0, curStart - 50);
            const fe = curStart + FETCH_SIZE;
            for (const key of rangeCacheRef.current.keys()) {
              const [s, e] = key.split('-').map(Number);
              if (fs >= s && fe <= e) {
                rangeCacheRef.current.delete(key);
                break;
              }
            }
            // 执行 pending refresh（此时 isFetchingRef.current = false，可正常 fetch）
            await fetchRecordsRangeRef.current(curStart, 0);
          }
        }
      }
    }
  }, []);

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
        if (myId === searchCounterRef.current) {
          rangeCacheRef.current.set('0-50', response.results);
          setResultsOffset(0);
          setResults(response.results);
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

    const atBottom = end >= totalCountRef.current - 1;
    const doFetch = async () => {
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
        // 不递增 fetchCounterRef，避免使正在进行的 fetch 失效。
        // 原因：fetchRecordsRange 完成后的 setTotalCount 会触发 useVirtualScroll
        // 重新计算，若 endIndex 改变则 onRangeChange 被触发，handleVisibleRangeChange
        // 会被调用。如果此时 ++fetchCounterRef，会使正在进行的 fetch（records-refresh
        // 触发的）结果被丢弃，形成空白窗口。缓存已被清除，下次 fetch 会获取最新数据。
      }
      if (isFetchingRef.current) {
        // 有正在进行的 fetch，标记 pending，等 fetch 完成后再刷新。
        // 不递增 fetchCounterRef，避免使正在进行的 fetch 失效。
        // fetchRecordsRange 的 finally 块会检查 pendingRefreshAfterFetchRef，
        // 自动执行 pending refresh 获取最新数据。
        pendingRefreshAfterFetchRef.current = true;
      } else {
        await fetchRecordsRangeRef.current(start, end);
      }
      // fetch 完成（数据已应用到 UI）后才允许 refresh 事件立即处理
      isDraggingRef.current = false;
    };

    if (atBottom) {
      // 到达底部时立即 fetch。
      // 不递增 fetchCounterRef，避免使正在进行的 fetch 失效。
      // 原因：fetchRecordsRange 完成后的 setTotalCount 会触发 useVirtualScroll
      // 重新计算，若 endIndex 改变则 onRangeChange 被触发，handleVisibleRangeChange
      // 会被调用。如果此时 ++fetchCounterRef，会使正在进行的 fetch（records-refresh
      // 触发的）结果被丢弃，形成空白窗口。
      // 如果有正在进行的 fetch，doFetch 会标记 pendingRefreshAfterFetchRef，
      // 等 fetch 完成后自动刷新。
      doFetch();
    } else {
      rangeChangeTimerRef.current = window.setTimeout(doFetch, 100);
    }
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
          searchQuery={searchState.query}
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
