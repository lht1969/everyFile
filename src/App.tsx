import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { save } from '@tauri-apps/plugin-dialog';
import SearchPanel from './components/SearchPanel';
import ResultList from './components/ResultList';
import StatusBar from './components/StatusBar';
import SettingsModal from './components/SettingsModal';
import './App.css';

interface SearchResult {
  file_id: number;
  name: string;
  path: string;
  size: number;
  modified_time: number;
  is_directory: boolean;
}

interface SearchResponse {
  total: number;
  results: SearchResult[];
}

interface RecordsRangeResponse {
  results: SearchResult[];
  total: number;
  start: number;
  end: number;
}

interface IndexStatus {
  status: string;
  file_count: number;
  progress: number;
  message: string;
  volumes: string[];
  last_update: string;
}

function App() {
  const [results, setResults] = useState<SearchResult[]>([]);
  const [totalCount, setTotalCount] = useState(0);
  const [statusMessage, setStatusMessage] = useState('');
  const [indexStatus, setIndexStatus] = useState<IndexStatus>({ status: 'ready', file_count: 0, progress: 1, message: '', volumes: [], last_update: '' });
  const [showSettings, setShowSettings] = useState(false);
  const [isAdmin, setIsAdmin] = useState(false);
  const [searchState, setSearchState] = useState({ query: '', filesOnly: true, directoriesOnly: false });
  const [sortState, setSortState] = useState({ field: 'name' as string, direction: 'asc' as string });
  const [scrollTrigger, setScrollTrigger] = useState(0);

  useEffect(() => {
    loadIndexStatus();
    checkAdmin();
    loadAllFiles();
  }, []);

  useEffect(() => {
    const unlistenProgress = listen<{ volume: string; count: number }>('scan-progress', (_event) => {
    });

    const unlistenComplete = listen<{ volume: string; count: number }>('scan-complete', (_event) => {
      loadIndexStatus();
    });

    const unlistenUpdated = listen<{ volume: string; count: number }>('index-updated', (_event) => {
      loadIndexStatus();
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
    try {
      const response = await invoke<SearchResponse>('search_files', {
        params: { query: '', files_only: true, sort_by: sortState.field, sort_direction: sortState.direction }
      });
      setTotalCount(response.total);
      if (response.results.length > 0) {
        setResults(response.results);
      } else if (response.total > 0) {
        const range = await invoke<RecordsRangeResponse>('get_records_range', { start: 0, end: 50 });
        setResults(range.results);
      }
    } catch (e) {
      console.error('Failed to load all files:', e);
    }
  };

  const fetchCounterRef = useRef(0);

  const fetchRecordsRange = useCallback(async (start: number, end: number) => {
    const myId = ++fetchCounterRef.current;
    try {
      const response = await invoke<RecordsRangeResponse>('get_records_range', { start, end });
      if (myId === fetchCounterRef.current) {
        setResults(response.results);
      }
    } catch (e) {
      console.error('Failed to fetch records range:', e);
    }
  }, []);

  const handleSearch = useCallback(async (searchQuery: string, filesOnly?: boolean, directoriesOnly?: boolean) => {
    setSearchState({ query: searchQuery, filesOnly: filesOnly ?? true, directoriesOnly: directoriesOnly ?? false });
    setScrollTrigger(prev => prev + 1);

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
      setTotalCount(response.total);
      if (response.results.length > 0) {
        setResults(response.results);
      } else if (response.total > 0) {
        const range = await invoke<RecordsRangeResponse>('get_records_range', { start: 0, end: 50 });
        setResults(range.results);
      } else {
        setResults([]);
      }
      setStatusMessage(searchQuery.trim() ? `找到 ${response.total} 个结果` : '');
    } catch (e) {
      console.error('Search failed:', e);
    }
  }, [sortState]);

  const handleSortChange = useCallback(async (field: string, direction: string) => {
    setSortState({ field, direction });
    setScrollTrigger(prev => prev + 1);
    try {
      const response = await invoke<SearchResponse>('search_files', {
        params: {
          query: searchState.query,
          files_only: searchState.filesOnly,
          directories_only: searchState.directoriesOnly,
          sort_by: field,
          sort_direction: direction
        }
      });
      setTotalCount(response.total);
      if (response.results.length > 0) {
        setResults(response.results);
      } else if (response.total > 0) {
        const range = await invoke<RecordsRangeResponse>('get_records_range', { start: 0, end: 50 });
        setResults(range.results);
      } else {
        setResults([]);
      }
    } catch (e) {
      console.error('Sort failed:', e);
    }
  }, [searchState]);

  const handleVisibleRangeChange = useCallback(async (start: number, end: number) => {
    fetchRecordsRange(start, end);
  }, [fetchRecordsRange]);

  const handleOpenFile = async (path: string) => {
    try {
      await invoke('open_file', { path });
    } catch (e) {
      console.error('Failed to open file:', e);
    }
  };

  const handleOpenFolder = async (path: string) => {
    try {
      await invoke('open_folder', { path });
    } catch (e) {
      console.error('Failed to open folder:', e);
    }
  };

  const handleDeleteFile = async (path: string) => {
    if (!confirm(`确定要删除 "${path}" 吗？`)) return;
    try {
      await invoke('delete_file', { path });
      loadAllFiles();
    } catch (e) {
      console.error('Failed to delete file:', e);
    }
  };

  const handleRebuildIndex = async () => {
    try {
      await invoke('rebuild_index');
      await loadIndexStatus();
      loadAllFiles();
    } catch (e) {
      console.error('Failed to rebuild index:', e);
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
          totalCount={totalCount}
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
          key={Date.now()}
          onClose={() => setShowSettings(false)}
          onRebuildIndex={handleRebuildIndex}
          indexStatus={indexStatus}
          onVolumeChange={() => loadAllFiles()}
        />
      )}
    </div>
  );
}

export default App;
