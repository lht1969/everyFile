import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { VolumeResponse, ConfigResponse, IndexStatus } from '../types';
import { formatSize } from '../utils/format';

interface SettingsModalProps {
  onClose: () => void;
  onRebuildIndex: () => void;
  indexStatus: IndexStatus;
  onVolumeChange?: () => void;
  rebuilding: boolean;
}

function SettingsModal({ onClose, onRebuildIndex, indexStatus, onVolumeChange, rebuilding }: SettingsModalProps) {
  const [volumes, setVolumes] = useState<VolumeResponse[]>([]);
  const [monitoredVolumes, setMonitoredVolumes] = useState<VolumeResponse[]>([]);
  const [config, setConfig] = useState<ConfigResponse | null>(null);

  useEffect(() => {
    loadVolumes();
    loadConfig();
  }, []);

  const loadVolumes = async () => {
    try {
      const allVolumes = await invoke<VolumeResponse[]>('get_volumes');
      setVolumes(allVolumes);
      const monitored = await invoke<VolumeResponse[]>('get_monitored_volumes');
      setMonitoredVolumes(monitored);
    } catch (e) {
      console.error('Failed to load volumes:', e);
    }
  };

  const loadConfig = async () => {
    try {
      const cfg = await invoke<ConfigResponse>('get_config');
      setConfig(cfg);
    } catch (e) {
      console.error('Failed to load config:', e);
    }
  };

  const handleAddVolume = async (volume: string) => {
    try {
      await invoke('add_volume', { volume });
      const allVolumes = await invoke<VolumeResponse[]>('get_volumes');
      setVolumes(allVolumes);
      const monitored = await invoke<VolumeResponse[]>('get_monitored_volumes');
      setMonitoredVolumes(monitored);
      if (onVolumeChange) onVolumeChange();
    } catch (e) {
      console.error('Failed to add volume:', e);
    }
  };

  const handleRemoveVolume = async (volume: string) => {
    try {
      await invoke('remove_volume', { volume });
      const allVolumes = await invoke<VolumeResponse[]>('get_volumes');
      setVolumes(allVolumes);
      const monitored = await invoke<VolumeResponse[]>('get_monitored_volumes');
      setMonitoredVolumes(monitored);
      if (onVolumeChange) onVolumeChange();
    } catch (e) {
      console.error('Failed to remove volume:', e);
    }
  };

  const handleSaveConfig = async () => {
    if (!config) return;

    const { monitored_volumes, ...configWithoutVolumes } = config;
    const configToSave = {
      ...configWithoutVolumes,
      monitored_volumes: monitoredVolumes.map(v => v.drive_letter),
    };

    try {
      await invoke('save_config', {
        params: {
          scan_all_volumes: configToSave.scan_all_volumes,
          default_volume: configToSave.default_volume,
          max_cache_items: configToSave.max_cache_items,
          max_history_items: configToSave.max_history_items,
          enable_usn_journal: configToSave.enable_usn_journal,
          include_hidden_files: configToSave.include_hidden_files,
          include_system_files: configToSave.include_system_files,
          update_interval: configToSave.update_interval,
          monitored_volumes: configToSave.monitored_volumes,
          startup: configToSave.startup
        }
      });
      onClose();
    } catch (error) {
      console.error('Failed to save config:', error);
    }
  };

  return (
    <div className={`modal-overlay${rebuilding ? ' rebuilding' : ''}`} onClick={rebuilding ? undefined : onClose}>
      <div className="modal-content" onClick={e => e.stopPropagation()}>
        <div className="modal-header">
          <h2>设置</h2>
          {!rebuilding && (
            <button className="close-button" onClick={onClose}>×</button>
          )}
        </div>

        <div className="modal-body">
          <section className="settings-section">
            <h3>卷管理</h3>
            <div className="volume-list">
              <div className="volume-group">
                <h4>已监控卷</h4>
                {monitoredVolumes.length === 0 ? (
                  <p className="empty-message">暂无监控的卷</p>
                ) : (
                  <ul>
                    {monitoredVolumes.map((v: VolumeResponse) => (
                      <li key={v.drive_letter}>
                        <span>{v.drive_letter} - {v.file_count} 个文件</span>
                        <button onClick={() => handleRemoveVolume(v.drive_letter)}>移除</button>
                      </li>
                    ))}
                  </ul>
                )}
              </div>

              <div className="volume-group">
                <h4>可用卷</h4>
                {volumes
                  .filter(v => !monitoredVolumes.some(m => m.drive_letter === v.drive_letter))
                  .map(v => (
                    <li key={v.drive_letter}>
                      <span>{v.drive_letter} - {v.volume_name || '本地卷'} ({v.file_system})</span>
                      <span className="volume-size">{formatSize(v.free_space)} 可用</span>
                      <button onClick={() => handleAddVolume(v.drive_letter)}>添加</button>
                    </li>
                  ))}
              </div>
            </div>
          </section>

          {config && (
            <section className="settings-section">
              <h3>索引设置</h3>
              <label>
                <input
                  type="checkbox"
                  checked={config.enable_usn_journal}
                  onChange={e => setConfig({ ...config, enable_usn_journal: e.target.checked })}
                />
                启用 USN Journal 监控（需要管理员权限）
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={config.include_hidden_files}
                  onChange={e => setConfig({ ...config, include_hidden_files: e.target.checked })}
                />
                包含隐藏文件
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={config.include_system_files}
                  onChange={e => setConfig({ ...config, include_system_files: e.target.checked })}
                />
                包含系统文件
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={config.scan_all_volumes}
                  onChange={e => setConfig({ ...config, scan_all_volumes: e.target.checked })}
                />
                扫描所有卷
              </label>
            </section>
          )}

          {config && (
            <section className="settings-section">
              <h3>系统设置</h3>
              <label>
                <input
                  type="checkbox"
                  checked={config.startup}
                  onChange={async (e) => {
                    setConfig({ ...config, startup: e.target.checked });
                    try {
                      if (e.target.checked) {
                        await invoke('add_startup');
                      } else {
                        await invoke('remove_startup');
                      }
                    } catch (e) {
                      console.error('Failed to set startup:', e);
                      setConfig(prev => prev ? { ...prev, startup: false } : prev);
                    }
                  }}
                />
                开机启动
              </label>
              <p className="setting-hint">开启后，程序将随系统启动并以静默模式运行</p>
            </section>
          )}

          <section className="settings-section">
            <h3>索引管理</h3>
            <p>已索引文件: {indexStatus.file_count} 个</p>
            <p>索引状态: {indexStatus.message}</p>
            {config && (
              <div className="interval-setting">
                <label className="interval-label" htmlFor="rebuild-interval">
                  后台定时重建间隔（分钟）：
                </label>
                <input
                  id="rebuild-interval"
                  className="interval-input"
                  type="number"
                  min={0}
                  max={3600}
                  value={config.update_interval}
                  onChange={(e) => {
                    const seconds = Math.max(0, parseInt(e.target.value) || 0);
                    setConfig({ ...config, update_interval: seconds });
                  }}
                />
                <span className="interval-hint">秒，设为 0 表示不自动更新</span>
              </div>
            )}
            <button className="rebuild-button" onClick={onRebuildIndex} disabled={rebuilding}>
              {rebuilding ? '正在重建索引...' : '立即重建索引'}
            </button>

          </section>
        </div>

        <div className="modal-footer">
          <button type="button" className="cancel-button" onClick={onClose} disabled={rebuilding}>取消</button>
          <button type="button" className="save-button" onClick={handleSaveConfig} disabled={rebuilding}>保存</button>
        </div>
      </div>
    </div>
  );
}

export default SettingsModal;
