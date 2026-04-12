<script setup lang="ts">
import { ref, onMounted } from 'vue';

interface VolumeResponse {
  drive_letter: string;
  volume_name: string;
  file_system: string;
  total_size: number;
  free_space: number;
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
}

interface Props {
  indexStatus: {
    status: string;
    file_count: number;
    progress: number;
    message: string;
  };
}

const props = defineProps<Props>();
const emit = defineEmits(['close', 'rebuildIndex']);

const volumes = ref<VolumeResponse[]>([]);
const monitoredVolumes = ref<string[]>([]);
const config = ref<ConfigResponse | null>(null);

onMounted(async () => {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const allVolumes = await invoke<VolumeResponse[]>('get_volumes');
    volumes.value = allVolumes;
    monitoredVolumes.value = allVolumes.map(v => v.drive_letter);
    
    const cfg = await invoke<ConfigResponse>('get_config');
    config.value = cfg;
  } catch (e) {
    console.error('Failed to load data:', e);
  }
});

const handleAddVolume = async (volume: string) => {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('add_volume', { volume });
    monitoredVolumes.value = [...monitoredVolumes.value, volume];
  } catch (e) {
    console.error('Failed to add volume:', e);
  }
};

const handleRemoveVolume = async (volume: string) => {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('remove_volume', { volume });
    monitoredVolumes.value = monitoredVolumes.value.filter(v => v !== volume);
  } catch (e) {
    console.error('Failed to remove volume:', e);
  }
};

const handleSaveConfig = async () => {
  if (!config.value) return;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('save_config', { config: config.value });
    emit('close');
  } catch (e) {
    console.error('Failed to save config:', e);
  }
};

const formatSize = (bytes: number) => {
  const gb = bytes / (1024 * 1024 * 1024);
  return `${gb.toFixed(1)} GB`;
};
</script>

<template>
  <div class="vue-settings-modal">
    <div class="modal-header">
      <h2>设置 (Vue)</h2>
      <button class="close-button" @click="emit('close')">×</button>
    </div>
    
    <div class="modal-body">
      <section class="settings-section">
        <h3>卷管理 (Vue)</h3>
        <div class="volume-list">
          <div class="volume-group">
            <h4>已监控卷</h4>
            <ul v-if="monitoredVolumes.length > 0">
              <li v-for="v in monitoredVolumes" :key="v">
                <span>{{ v }}</span>
                <button @click="handleRemoveVolume(v)">移除</button>
              </li>
            </ul>
            <p v-else class="empty-message">暂无监控的卷</p>
          </div>
          
          <div class="volume-group">
            <h4>可用卷</h4>
            <ul>
              <li v-for="v in volumes.filter(x => !monitoredVolumes.includes(x.drive_letter))" :key="v.drive_letter">
                <span>{{ v.drive_letter }} - {{ v.volume_name || '本地卷' }} ({{ v.file_system }})</span>
                <span class="volume-size">{{ formatSize(v.free_space) }} 可用</span>
                <button @click="handleAddVolume(v.drive_letter)">添加</button>
              </li>
            </ul>
          </div>
        </div>
      </section>

      <section v-if="config" class="settings-section">
        <h3>索引设置</h3>
        <label>
          <input type="checkbox" v-model="config.enable_usn_journal" />
          启用 USN Journal 监控
        </label>
        <label>
          <input type="checkbox" v-model="config.include_hidden_files" />
          包含隐藏文件
        </label>
        <label>
          <input type="checkbox" v-model="config.include_system_files" />
          包含系统文件
        </label>
        <label>
          <input type="checkbox" v-model="config.scan_all_volumes" />
          扫描所有卷
        </label>
      </section>

      <section class="settings-section">
        <h3>索引管理</h3>
        <p>已索引文件: {{ props.indexStatus.file_count }} 个</p>
        <button class="rebuild-button" @click="emit('rebuildIndex')">重建索引</button>
      </section>
    </div>

    <div class="modal-footer">
      <button class="cancel-button" @click="emit('close')">取消</button>
      <button class="save-button" @click="handleSaveConfig">保存</button>
    </div>
  </div>
</template>

<style scoped>
.vue-settings-modal {
  background: #fff;
  border-radius: 12px;
  width: 90%;
  max-width: 600px;
  max-height: 80vh;
  display: flex;
  flex-direction: column;
  box-shadow: 0 20px 25px -5px rgba(0, 0, 0, 0.1);
}

.modal-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 16px 20px;
  border-bottom: 1px solid #e5e7eb;
}

.modal-header h2 {
  font-size: 18px;
  font-weight: 600;
}

.close-button {
  width: 32px;
  height: 32px;
  border: none;
  background: transparent;
  font-size: 24px;
  cursor: pointer;
  color: #6b7280;
  border-radius: 6px;
}

.close-button:hover {
  background: #f3f4f6;
}

.modal-body {
  flex: 1;
  padding: 20px;
  overflow-y: auto;
}

.settings-section {
  margin-bottom: 24px;
}

.settings-section h3 {
  font-size: 16px;
  font-weight: 600;
  margin-bottom: 12px;
}

.volume-list {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.volume-group h4 {
  font-size: 14px;
  font-weight: 500;
  margin-bottom: 8px;
  color: #6b7280;
}

.volume-group ul {
  list-style: none;
}

.volume-group li {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px;
  border: 1px solid #e5e7eb;
  border-radius: 6px;
  margin-bottom: 8px;
}

.volume-group li button {
  margin-left: auto;
  padding: 4px 12px;
  border: 1px solid #e5e7eb;
  border-radius: 4px;
  background: #fff;
  cursor: pointer;
  font-size: 12px;
}

.volume-group li button:hover {
  background: #f3f4f6;
}

.volume-size {
  font-size: 12px;
  color: #6b7280;
}

.empty-message {
  color: #6b7280;
  font-size: 13px;
}

.settings-section label {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 12px;
  cursor: pointer;
}

.settings-section input[type="checkbox"] {
  width: 16px;
  height: 16px;
}

.rebuild-button {
  padding: 8px 16px;
  border: 1px solid #e5e7eb;
  border-radius: 6px;
  background: #fff;
  cursor: pointer;
  margin-right: 8px;
}

.rebuild-button:hover {
  background: #f3f4f6;
  border-color: #2563eb;
}

.modal-footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 16px 20px;
  border-top: 1px solid #e5e7eb;
}

.cancel-button,
.save-button {
  padding: 8px 16px;
  border-radius: 6px;
  cursor: pointer;
  font-size: 14px;
}

.cancel-button {
  border: 1px solid #e5e7eb;
  background: #fff;
}

.cancel-button:hover {
  background: #f3f4f6;
}

.save-button {
  border: none;
  background: #2563eb;
  color: #fff;
}

.save-button:hover {
  background: #1d4ed8;
}
</style>