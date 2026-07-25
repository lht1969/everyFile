export interface SearchResult {
  file_id: number;
  name: string;
  path: string;
  size: number;
  modified_time: number;
  is_directory: boolean;
}

export interface SearchResponse {
  total: number;
  results: SearchResult[];
}

export interface RecordsRangeResponse {
  results: SearchResult[];
  total: number;
  start: number;
  end: number;
}

export interface VolumeStatus {
  drive_letter: string;
  state: VolumeState;
}

export type VolumeState = 
  | { type: 'Loading' }
  | { type: 'Ready'; file_count: number }
  | { type: 'Error'; message: string };

export interface IndexStatus {
  status: string;
  file_count: number;
  progress: number;
  message: string;
  volumes: string[];
  last_update: string;
  scanning_volumes: string[];
  volume_statuses: VolumeStatus[];
}

export interface AppConfig {
  update_interval: number;
}

export interface VolumeResponse {
  drive_letter: string;
  volume_name: string;
  file_system: string;
  total_size: number;
  free_space: number;
  file_count: number;
}

export interface ConfigResponse {
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

export type SortField = 'name' | 'size' | 'modified_time' | 'path';
export type SortDirection = 'asc' | 'desc';
