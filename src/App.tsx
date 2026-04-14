import { useState, useEffect, useCallback } from 'react';
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
  modified_time: string;
  is_directory: boolean;
  formatted_size: string;
  formatted_modified_time: string;
}

interface SearchResponse {
  results: SearchResult[];
  total: number;
  page: number;
  page_size: number;
  total_pages: number;
}

interface IndexStatus {
  status: string;
  file_count: number;
  progress: number;
  message: string;
}

function App() {
  const [results, setResults] = useState<SearchResult[]>([]);
  const [statusMessage, setStatusMessage] = useState('就绪');
  const [indexStatus, setIndexStatus] = useState<IndexStatus>({ status: 'ready', file_count: 0, progress: 1, message: '' });
  const [showSettings, setShowSettings] = useState(false);
  const [isAdmin, setIsAdmin] = useState(false);
  const [pagination, setPagination] = useState({ page: 1, total: 0, total_pages: 0 });
  const [searchState, setSearchState] = useState({ query: '', filesOnly: true, directoriesOnly: false });

  useEffect(() => {
    loadIndexStatus();
    checkAdmin();
    loadAllFiles();
  }, []);

  useEffect(() => {
    const unlistenProgress = listen<{ volume: string; count: number }>('scan-progress', (event) => {
      setStatusMessage(`正在扫描${event.payload.volume}卷，已索引 ${event.payload.count} 个文件...`);
    });

    const unlistenComplete = listen<{ volume: string; count: number }>('scan-complete', (event) => {
      setStatusMessage(`${event.payload.volume}卷扫描结束，共 ${event.payload.count} 个文件`);
      loadIndexStatus();
    });

    const unlistenUpdated = listen<{ volume: string; count: number }>('index-updated', (event) => {
      setStatusMessage(`索引更新: ${event.payload.volume}卷新增 ${event.payload.count} 个文件`);
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
      setStatusMessage(status.message);
    } catch (e) {
      console.error('Failed to load index status:', e);
    }
  };

  const loadAllFiles = async () => {
    try {
      const response = await invoke<SearchResponse>('search_files', {
        params: { query: '', page: 1, page_size: 1000, files_only: true }
      });
      setResults(response.results);
      setPagination({ page: response.page, total: response.total, total_pages: response.total_pages });
      setStatusMessage(`显示 ${response.results.length} 个结果 (共 ${response.total} 个)`);
    } catch (e) {
      console.error('Failed to load all files:', e);
    }
  };

  const handleSearch = useCallback(async (searchQuery: string, filesOnly?: boolean, directoriesOnly?: boolean) => {
    setStatusMessage(`正在搜索: ${searchQuery}`);

    setSearchState({ query: searchQuery, filesOnly: filesOnly ?? true, directoriesOnly: directoriesOnly ?? false });

    try {
      const response = await invoke<SearchResponse>('search_files', {
        params: {
          query: searchQuery,
          page: 1,
          page_size: 1000,
          files_only: filesOnly,
          directories_only: directoriesOnly
        }
      });
      setResults(response.results);
      setPagination({ page: response.page, total: response.total, total_pages: response.total_pages });
      setStatusMessage(`找到 ${response.total} 个结果`);
    } catch (e) {
      console.error('Search failed:', e);
      setStatusMessage(`搜索失败: ${e}`);
    }
  }, []);

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
      setStatusMessage(`已删除: ${path}`);
      loadAllFiles();
    } catch (e) {
      console.error('Failed to delete file:', e);
      setStatusMessage(`删除失败: ${e}`);
    }
  };

  const handleRebuildIndex = async () => {
    setStatusMessage('正在重建索引...');
    try {
      await invoke('rebuild_index');
      await loadIndexStatus();
      loadAllFiles();
      setStatusMessage('索引重建完成');
    } catch (e) {
      console.error('Failed to rebuild index:', e);
      setStatusMessage(`索引重建失败: ${e}`);
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
        setStatusMessage('导出已取消');
        return;
      }

      if (pagination.total > 1000) {
        await invoke('export_all_results', {
          query: searchState.query,
          filesOnly: searchState.filesOnly,
          directoriesOnly: searchState.directoriesOnly,
          format,
          path
        });
        setStatusMessage(`已导出全部 ${pagination.total} 条结果到 ${path}`);
      } else {
        if (format === 'csv') {
          await invoke('export_csv', { results, path });
        } else if (format === 'txt') {
          await invoke('export_txt', { results, path });
        } else {
          await invoke('export_json', { results, path });
        }
        setStatusMessage(`已导出到 ${path}`);
      }
    } catch (e) {
      console.error('Export failed:', e);
      setStatusMessage(`导出失败: ${e}`);
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
          onOpenFile={handleOpenFile}
          onOpenFolder={handleOpenFolder}
          onDeleteFile={handleDeleteFile}
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