import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';

// 图标缓存上限：128 个条目，覆盖常见扩展名/目录，避免无界增长导致内存占用持续升高
const ICON_CACHE_LIMIT = 128;

const cache = new Map<string, string>();
const pending = new Set<string>();
type Listener = () => void;
const listeners = new Map<string, Set<Listener>>();

function subscribe(key: string, listener: Listener): () => void {
  if (!listeners.has(key)) listeners.set(key, new Set());
  listeners.get(key)!.add(listener);
  return () => { listeners.get(key)?.delete(listener); };
}

function notify(key: string) {
  listeners.get(key)?.forEach(fn => fn());
}

/**
 * 读取缓存并更新 LRU 顺序（命中条目移到最近使用端）。
 */
function getCachedIcon(key: string): string | undefined {
  const url = cache.get(key);
  if (url !== undefined) {
    // 命中：先删除再插入，使其位于 Map 末尾（最近使用）
    cache.delete(key);
    cache.set(key, url);
  }
  return url;
}

/**
 * 写入缓存并执行 LRU 淘汰。
 * 优先淘汰没有活跃订阅者的条目，避免正在显示的图标被清掉导致重复 IPC。
 */
function setCachedIcon(key: string, url: string) {
  cache.delete(key);
  cache.set(key, url);

  if (cache.size <= ICON_CACHE_LIMIT) return;

  // 第一回合：淘汰没有 listener 的条目
  for (const oldestKey of cache.keys()) {
    if (cache.size <= ICON_CACHE_LIMIT) return;
    if ((listeners.get(oldestKey)?.size ?? 0) === 0) {
      cache.delete(oldestKey);
    }
  }

  // 若仍超限，淘汰最久未使用的条目
  if (cache.size > ICON_CACHE_LIMIT) {
    const oldestKey = cache.keys().next().value;
    if (oldestKey !== undefined) {
      cache.delete(oldestKey);
    }
  }
}

async function loadIcon(key: string, filePath: string, isDirectory: boolean) {
  if (pending.has(key)) return;
  pending.add(key);
  try {
    const url = await invoke<string>('get_file_icon', { filePath, isDirectory });
    setCachedIcon(key, url);
    notify(key);
  } catch {
    setCachedIcon(key, '');
    notify(key);
  } finally {
    pending.delete(key);
  }
}

function getKey(path: string, isDirectory: boolean): string {
  if (isDirectory) return '__directory__';
  const idx = path.lastIndexOf('.');
  return idx === -1 ? '__noext__' : path.substring(idx).toLowerCase();
}

export function useFileIcon(path: string, isDirectory: boolean): string | undefined {
  const key = getKey(path, isDirectory);
  const [url, setUrl] = useState<string | undefined>(() => getCachedIcon(key));

  useEffect(() => {
    if (cache.has(key)) {
      setUrl(getCachedIcon(key));
      return;
    }

    setUrl(undefined);

    const unsub = subscribe(key, () => {
      setUrl(getCachedIcon(key));
    });

    loadIcon(key, path, isDirectory);

    return unsub;
  }, [key, path, isDirectory]);

  return url;
}
