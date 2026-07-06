import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface VolumeResponse {
  drive_letter: string;
  volume_name: string;
  file_system: string;
  total_size: number;
  free_space: number;
  file_count: number;
}

interface ConfigResponse {
  scan_all_volumes: boolean;
  default_volume: string;
  max_cache_items: number;
  max_history_items: number;
  enable_usn_journal: boolean;
  include_hidden_files: boolean;
  include_system_files: boolean;
  update_interval: number;
  monitored_volumes: string[];
  startup: boolean;
}

interface IndexStatus {
  status: string;
  file_count: number;
  progress: number;
  message: string;
}

interface SettingsModalProps {
  onClose: () => void;
  onRebuildIndex: () => void;
  indexStatus: IndexStatus;
  onVolumeChange?: () => void;
}

function SettingsModal({ onClose, onRebuildIndex, indexStatus, onVolumeChange }: SettingsModalProps) {
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
      alert('加载卷失败: ' + e);
      console.error('Failed to load volumes:', e);
    }
  };

  const loadConfig = async () => {
    try {
      const cfg = await invoke<ConfigResponse>('get_config');
      setConfig(cfg);
    } catch (e) {
      alert('加载配置失败: ' + e);
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

  const handleSaveConfig = () => {
    console.log('handleSaveConfig called');
    if (!config) {
      alert('配置正在加载中，请稍后再试');
      return;
    }

    console.log('Config before processing:', config);
    const { monitored_volumes, ...configWithoutVolumes } = config;
    const configToSave = {
      ...configWithoutVolumes,
      monitored_volumes: monitoredVolumes.map(v => v.drive_letter),
    };
    console.log('Saving config:', configToSave);

    // 调用 save_config 命令，传递用户修改后的配置
    console.log('Before invoke save_config');
    invoke('save_config', {
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
    })
      .then(result => {
        console.log('Save config result:', result);
        console.log('Config saved successfully');
        console.log('Calling onClose()');
        onClose();
        console.log('onClose() called');
      })
      .catch(error => {
        console.error('Error invoking save_config:', error);
        console.error('Error details:', JSON.stringify(error, null, 2));
        alert('保存失败: ' + error);
      });
  };

  const formatSize = (bytes: number) => {
    const gb = bytes / (1024 * 1024 * 1024);
    return `${gb.toFixed(1)} GB`;
  };

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal-content" onClick={e => e.stopPropagation()}>
        <div className="modal-header">
          <h2>设置</h2>
          <button className="close-button" onClick={onClose}>×</button>
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
                        alert('开机启动已开启');
                      } else {
                        await invoke('remove_startup');
                        alert('开机启动已关闭');
                      }
                    } catch (e) {
                      alert('设置开机启动失败: ' + e);
                      console.error('Failed to set startup:', e);
                      setConfig({ ...config, startup: false });
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
            <button className="rebuild-button" onClick={onRebuildIndex}>
              重建索引
            </button>

          </section>
        </div>

        <div className="modal-footer">
          <button type="button" className="cancel-button" onClick={onClose}>取消</button>
          <button type="button" className="save-button" onClick={handleSaveConfig}>保存</button>
        </div>
      </div>
    </div>
  );
}

export default SettingsModal;