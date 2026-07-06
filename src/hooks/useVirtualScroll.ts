import { useState, useEffect, useCallback, useRef } from 'react';

interface UseVirtualScrollOptions {
  totalItems: number;
  itemHeight: number;
  overscan?: number;
  containerRef: React.RefObject<HTMLDivElement>;
  onRangeChange?: (startIndex: number, endIndex: number) => void;
}

interface UseVirtualScrollReturn {
  startIndex: number;
  endIndex: number;
  offsetY: number;
  spacerHeight: number;
  visibleItems: number;
  scrollToIndex: (index: number) => void;
}

export function useVirtualScroll({
  totalItems,
  itemHeight,
  overscan = 5,
  containerRef,
  onRangeChange,
}: UseVirtualScrollOptions): UseVirtualScrollReturn {
  const [scrollTop, setScrollTop] = useState(0);
  const rafId = useRef<number | null>(null);
  const lastFiredRef = useRef<string>('');
  const onRangeChangeRef = useRef(onRangeChange);
  onRangeChangeRef.current = onRangeChange;

  const startIndex = Math.floor(scrollTop / itemHeight);
  const viewportHeight = containerRef.current?.clientHeight ?? 0;
  const visibleCount = Math.ceil(viewportHeight / itemHeight) + overscan;
  const endIndex = Math.min(startIndex + visibleCount, totalItems);
  const offsetY = startIndex * itemHeight;
  const spacerHeight = totalItems * itemHeight;

  useEffect(() => {
    const key = `${startIndex}-${endIndex}`;
    if (key !== lastFiredRef.current) {
      lastFiredRef.current = key;
      onRangeChangeRef.current?.(startIndex, endIndex);
    }
  }, [startIndex, endIndex]);

  const handleScroll = useCallback(() => {
    if (rafId.current !== null) {
      cancelAnimationFrame(rafId.current);
    }
    rafId.current = requestAnimationFrame(() => {
      if (containerRef.current) {
        setScrollTop(containerRef.current.scrollTop);
      }
      rafId.current = null;
    });
  }, [containerRef]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    container.addEventListener('scroll', handleScroll, { passive: true });
    return () => {
      container.removeEventListener('scroll', handleScroll);
      if (rafId.current !== null) {
        cancelAnimationFrame(rafId.current);
      }
    };
  }, [containerRef, handleScroll]);

  const scrollToIndex = useCallback((index: number) => {
    if (containerRef.current) {
      containerRef.current.scrollTop = index * itemHeight;
    }
  }, [containerRef, itemHeight]);

  return {
    startIndex,
    endIndex,
    offsetY,
    spacerHeight,
    visibleItems: endIndex - startIndex,
    scrollToIndex,
  };
}
