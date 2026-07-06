import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';

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

async function loadIcon(key: string, filePath: string, isDirectory: boolean) {
  if (pending.has(key)) return;
  pending.add(key);
  try {
    const url = await invoke<string>('get_file_icon', { filePath, isDirectory });
    cache.set(key, url);
    notify(key);
  } catch {
    cache.set(key, '');
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
  const [url, setUrl] = useState<string | undefined>(() => cache.get(key));

  useEffect(() => {
    if (cache.has(key)) {
      setUrl(cache.get(key));
      return;
    }

    setUrl(undefined);

    const unsub = subscribe(key, () => {
      setUrl(cache.get(key));
    });

    loadIcon(key, path, isDirectory);

    return unsub;
  }, [key, path, isDirectory]);

  return url;
}
