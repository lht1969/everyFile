import { useState, useRef, useCallback, useEffect } from 'react';
import type { SearchResult } from '../types';

interface ColumnConfig {
  naturalWidth: number;
  minWidth: number;
  fixed: boolean;
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

function measureNaturalWidths(results: SearchResult[]): { name: number; path: number } {
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
    name: Math.max(120, Math.min(maxNameW + NAME_ICON_WIDTH + 16, 600)),
    path: Math.max(80, Math.min(maxPathW + 16, 800)),
  };
}

function calcWidths(available: number, cols: ColumnConfig[]): number[] {
  const totalNatural = cols.reduce((s, c) => s + c.naturalWidth, 0);

  if (available >= totalNatural) {
    const surplus = available - totalNatural;
    return expandCols(surplus, cols);
  } else {
    const deficit = totalNatural - available;
    return shrinkCols(deficit, cols);
  }
}

function shrinkCols(deficit: number, cols: ColumnConfig[]): number[] {
  let widths = cols.map(c => c.naturalWidth);
  let remaining = deficit;

  // Phase 1: compress non-fixed (name, path)
  for (let phase = 0; phase < 2 && remaining > 0; phase++) {
    const phaseIndices = cols
      .map((c, i) => ({ c, i }))
      .filter(x => !x.c.fixed && (phase === 0 ? x.c.minWidth > 0 : true))
      .map(x => x.i);

    if (phaseIndices.length === 0) continue;

    const phaseTotal = phaseIndices.reduce((s, i) => s + widths[i], 0);
    const phaseMin = phaseIndices.reduce((s, i) => s + cols[i].minWidth, 0);
    const phaseNeed = Math.min(remaining, Math.max(0, phaseTotal - phaseMin));

    if (phaseNeed <= 0) continue;

    const ratio = phaseNeed / phaseTotal;
    let allocated = 0;
    for (let j = 0; j < phaseIndices.length; j++) {
      const idx = phaseIndices[j];
      const shrink = j === phaseIndices.length - 1
        ? phaseNeed - allocated
        : Math.floor(widths[idx] * ratio);
      widths[idx] = Math.max(cols[idx].minWidth, widths[idx] - shrink);
      allocated += shrink;
    }
    remaining -= phaseNeed;
  }

  // Phase 2: compress fixed (size, modified) as last resort
  if (remaining > 0) {
    const fixedIndices = cols.map((c, i) => ({ c, i })).filter(x => x.c.fixed).map(x => x.i);
    const fixedTotal = fixedIndices.reduce((s, i) => s + widths[i], 0);
    const fixedMin = fixedIndices.reduce((s, i) => s + cols[i].minWidth, 0);
    const fixedNeed = Math.min(remaining, Math.max(0, fixedTotal - fixedMin));

    if (fixedNeed > 0) {
      const ratio = fixedNeed / fixedTotal;
      let allocated = 0;
      for (let j = 0; j < fixedIndices.length; j++) {
        const idx = fixedIndices[j];
        const shrink = j === fixedIndices.length - 1
          ? fixedNeed - allocated
          : Math.floor(widths[idx] * ratio);
        widths[idx] = Math.max(cols[idx].minWidth, widths[idx] - shrink);
        allocated += shrink;
      }
    }
  }

  return widths;
}

function expandCols(surplus: number, cols: ColumnConfig[]): number[] {
  let widths = cols.map(c => c.naturalWidth);
  let remaining = surplus;

  // Expand non-fixed columns that are below natural
  const nonFixedIndices = cols.map((c, i) => ({ c, i })).filter(x => !x.c.fixed).map(x => x.i);

  // First pass: distribute evenly among non-fixed columns below natural
  if (remaining > 0 && nonFixedIndices.length > 0) {
    const needsSpace = nonFixedIndices.filter(i => widths[i] < cols[i].naturalWidth);
    if (needsSpace.length > 0) {
      const totalNeed = needsSpace.reduce((s, i) => s + (cols[i].naturalWidth - widths[i]), 0);
      const give = Math.min(remaining, totalNeed);
      const each = give / needsSpace.length;
      for (const i of needsSpace) {
        widths[i] = Math.min(cols[i].naturalWidth, widths[i] + each);
      }
      remaining -= give;
    }
  }

  // Second pass: if surplus remains, give to path only
  if (remaining > 0) {
    const pathIdx = nonFixedIndices.find(i => cols[i].minWidth === PATH_MIN_WIDTH);
    if (pathIdx !== undefined && widths[pathIdx] < cols[pathIdx].naturalWidth) {
      const need = cols[pathIdx].naturalWidth - widths[pathIdx];
      const give = Math.min(remaining, need);
      widths[pathIdx] += give;
      remaining -= give;
    }
  }

  return widths;
}

export function useColumnWidths(
  results: SearchResult[],
  containerRef: React.RefObject<HTMLDivElement | null>
) {
  const [naturalWidths, setNaturalWidths] = useState<{ name: number; path: number }>({ name: 0, path: 0 });
  const [columnWidths, setColumnWidths] = useState<number[]>([0, 0, SIZE_NATURAL, MODIFIED_NATURAL]);
  const containerWidthRef = useRef(0);
  const frozenColumnsRef = useRef<Set<number>>(new Set());

  const recalc = useCallback((containerWidth: number) => {
    containerWidthRef.current = containerWidth;

    // If natural widths not measured yet, use original 1fr/2fr proportions
    if (naturalWidths.name === 0 && naturalWidths.path === 0 && containerWidth > 0) {
      const fixedTotal = SIZE_NATURAL + MODIFIED_NATURAL;
      const avail = Math.max(0, containerWidth - fixedTotal);
      const nameW = Math.floor(avail / 3);
      const pathW = avail - nameW;
      setColumnWidths([nameW, pathW, SIZE_NATURAL, MODIFIED_NATURAL]);
      return;
    }

    const cols: ColumnConfig[] = [
      { naturalWidth: naturalWidths.name, minWidth: NAME_MIN_WIDTH, fixed: false },
      { naturalWidth: naturalWidths.path, minWidth: PATH_MIN_WIDTH, fixed: false },
      { naturalWidth: SIZE_NATURAL, minWidth: SIZE_MIN, fixed: true },
      { naturalWidth: MODIFIED_NATURAL, minWidth: MODIFIED_MIN, fixed: true },
    ];

    const raw = calcWidths(containerWidth, cols);

    // Respect frozen columns
    const result = raw.map((w, i) =>
      frozenColumnsRef.current.has(i) ? columnWidths[i] : w
    );

    setColumnWidths(result);
  }, [naturalWidths, columnWidths]);

  // Measure natural widths when results change
  useEffect(() => {
    const newNatural = measureNaturalWidths(results);
    setNaturalWidths(newNatural);
    // Unfreeze all columns on new results
    frozenColumnsRef.current.clear();
  }, [results]);

  // Recalculate when natural widths change
  useEffect(() => {
    if (containerWidthRef.current > 0) {
      recalc(containerWidthRef.current);
    }
  }, [naturalWidths, recalc]);

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
    setColumnWidths(prev => {
      const next = [...prev];
      next[index] = width;
      return next;
    });
    frozenColumnsRef.current.add(index);
  }, []);

  return {
    columnWidths,
    freezeColumn,
    unfreezeAll,
    setManualWidth,
  };
}
