import { useState, useRef, useCallback, useEffect } from 'react';
import type { SearchResult } from '../types';

interface ColumnConfig {
  naturalWidth: number;
  minWidth: number;
}

const NAME_ICON_WIDTH = 24;
const NAME_MIN_WIDTH = 100;
const PATH_MIN_WIDTH = 0;
const SIZE_NATURAL = 100;
const SIZE_MIN = 80;
const MODIFIED_NATURAL = 150;
const MODIFIED_MIN = 110;

function measureTextWidth(text: string, className: string): number {
  const el = document.createElement('div');
  el.style.cssText = 'position:absolute;visibility:hidden;white-space:nowrap;font-size:13px;font-family:system-ui,-apple-system,sans-serif;padding:0;margin:0;border:none;';
  if (className) el.className = className;
  el.textContent = text;
  document.body.appendChild(el);
  const w = el.getBoundingClientRect().width;
  document.body.removeChild(el);
  return w;
}

function measureContentWidths(results: SearchResult[]): { name: number; path: number } {
  if (results.length === 0) return { name: 0, path: 0 };

  let maxNameW = 0;
  let maxPathW = 0;
  const sampleSize = Math.min(results.length, 200);
  const step = Math.max(1, Math.floor(results.length / sampleSize));

  for (let i = 0; i < results.length; i += step) {
    const r = results[i];
    const nameW = measureTextWidth(r.name, 'col-name-text');
    if (nameW > maxNameW) maxNameW = nameW;

    const isDir = r.is_directory;
    const lastSlash = r.path.lastIndexOf('\\');
    const dirPath = isDir
      ? (r.path.endsWith('\\') ? r.path : r.path + '\\')
      : (lastSlash > 0 ? r.path.substring(0, lastSlash) : r.path);
    const pathW = measureTextWidth(dirPath, 'col-path');
    if (pathW > maxPathW) maxPathW = pathW;
  }

  return {
    name: maxNameW + NAME_ICON_WIDTH + 16,
    path: maxPathW + 16,
  };
}

function getProportionalWidths(containerWidth: number): { name: number; path: number } {
  const fixedTotal = SIZE_NATURAL + MODIFIED_NATURAL;
  const avail = Math.max(0, containerWidth - fixedTotal);
  return { name: Math.floor(avail / 3), path: avail - Math.floor(avail / 3) };
}

function shrinkCols(available: number, current: number[], cols: ColumnConfig[]): number[] {
  let widths = [...current];
  let deficit = widths.reduce((s, w) => s + w, 0) - available;
  if (deficit <= 0) return widths;

  // Phase 0: compress name only
  const nameShrink = Math.min(deficit, Math.max(0, widths[0] - cols[0].minWidth));
  widths[0] -= nameShrink;
  deficit -= nameShrink;

  // Phase 1: compress path only
  if (deficit > 0) {
    const pathShrink = Math.min(deficit, Math.max(0, widths[1] - cols[1].minWidth));
    widths[1] -= pathShrink;
    deficit -= pathShrink;
  }

  // Phase 2: compress fixed (size, modified) as last resort
  if (deficit > 0) {
    for (const idx of [2, 3]) {
      if (deficit <= 0) break;
      const canShrink = Math.min(deficit, Math.max(0, widths[idx] - cols[idx].minWidth));
      widths[idx] -= canShrink;
      deficit -= canShrink;
    }
  }

  return widths;
}

function expandCols(available: number, current: number[], cols: ColumnConfig[]): number[] {
  let widths = [...current];

  // Phase 0: compress columns above their natural width to free space
  // This handles inflated widths from a previous wider layout
  for (const idx of [0, 1, 2, 3]) {
    const over = widths[idx] - cols[idx].naturalWidth;
    if (over > 0) {
      widths[idx] = cols[idx].naturalWidth;
    }
  }

  let remaining = available - widths.reduce((s, w) => s + w, 0);
  if (remaining <= 0) return widths;

  // Phase 1: expand name to natural
  const nameNeed = cols[0].naturalWidth - widths[0];
  if (nameNeed > 0 && remaining > 0) {
    const give = Math.min(remaining, nameNeed);
    widths[0] += give;
    remaining -= give;
  }

  // Phase 2: expand path to natural
  const pathNeed = cols[1].naturalWidth - widths[1];
  if (pathNeed > 0 && remaining > 0) {
    const give = Math.min(remaining, pathNeed);
    widths[1] += give;
    remaining -= give;
  }

  // Phase 3: extra surplus → path continues expanding
  if (remaining > 0) {
    widths[1] += remaining;
  }

  return widths;
}

function calcWidths(available: number, current: number[], cols: ColumnConfig[]): number[] {
  const totalCurrent = current.reduce((s, w) => s + w, 0);
  // If current widths already exceed available space, must shrink from current
  if (totalCurrent > available) {
    return shrinkCols(available, current, cols);
  }
  // Otherwise expand from current toward natural targets
  return expandCols(available, current, cols);
}

export function useColumnWidths(
  results: SearchResult[],
  containerRef: React.RefObject<HTMLDivElement | null>
) {
  const [columnWidths, setColumnWidths] = useState<number[]>([0, 0, SIZE_NATURAL, MODIFIED_NATURAL]);
  const columnWidthsRef = useRef(columnWidths);
  const containerWidthRef = useRef(0);
  const frozenColumnsRef = useRef<Set<number>>(new Set());
  const contentWidthsRef = useRef<{ name: number; path: number }>({ name: 0, path: 0 });

  // Keep ref in sync with state
  columnWidthsRef.current = columnWidths;

  const recalc = useCallback((containerWidth: number) => {
    containerWidthRef.current = containerWidth;
    const current = columnWidthsRef.current;

    // Before data loads: use 1fr/2fr proportions
    if (contentWidthsRef.current.name === 0 && contentWidthsRef.current.path === 0 && containerWidth > 0) {
      const prop = getProportionalWidths(containerWidth);
      const next = [prop.name, prop.path, SIZE_NATURAL, MODIFIED_NATURAL];
      columnWidthsRef.current = next;
      setColumnWidths(next);
      return;
    }

    // Natural width = max(measured content, proportional) so columns follow window proportions
    const prop = getProportionalWidths(containerWidth);
    const effectiveNatural = {
      name: Math.max(contentWidthsRef.current.name, prop.name),
      path: Math.max(contentWidthsRef.current.path, prop.path),
    };

    const cols: ColumnConfig[] = [
      { naturalWidth: effectiveNatural.name, minWidth: NAME_MIN_WIDTH },
      { naturalWidth: effectiveNatural.path, minWidth: PATH_MIN_WIDTH },
      { naturalWidth: SIZE_NATURAL, minWidth: SIZE_MIN },
      { naturalWidth: MODIFIED_NATURAL, minWidth: MODIFIED_MIN },
    ];

    const raw = calcWidths(containerWidth, current, cols);

    const result = raw.map((w, i) =>
      frozenColumnsRef.current.has(i) ? current[i] : w
    );

    columnWidthsRef.current = result;
    setColumnWidths(result);
  }, []);

  // Store measured content widths when results change
  useEffect(() => {
    const measured = measureContentWidths(results);
    contentWidthsRef.current = measured;
    frozenColumnsRef.current.clear();
    // Trigger recalc with new content widths
    if (containerWidthRef.current > 0) {
      recalc(containerWidthRef.current);
    }
  }, [results, recalc]);

  // ResizeObserver
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const observer = new ResizeObserver(entries => {
      for (const entry of entries) {
        const w = entry.contentBoxSize?.[0]?.inlineSize ?? entry.contentRect.width;
        if (w > 0) {
          containerWidthRef.current = w;
          recalc(w);
        }
      }
    });

    observer.observe(container);
    return () => observer.disconnect();
  }, [containerRef, recalc]);

  const freezeColumn = useCallback((index: number) => {
    frozenColumnsRef.current.add(index);
  }, []);

  const unfreezeAll = useCallback(() => {
    frozenColumnsRef.current.clear();
  }, []);

  const setManualWidth = useCallback((index: number, width: number) => {
    const next = [...columnWidthsRef.current];
    next[index] = width;
    columnWidthsRef.current = next;
    setColumnWidths(next);
    frozenColumnsRef.current.add(index);
  }, []);

  return {
    columnWidths,
    freezeColumn,
    unfreezeAll,
    setManualWidth,
  };
}
