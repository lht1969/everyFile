import { useState, useEffect, useCallback, useRef } from 'react';

const SCROLL_SPACE = 30_000_000; // 浏览器安全 scrollHeight 上限内

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
  resetScroll: () => void;
}

export function useVirtualScroll({
  totalItems,
  itemHeight,
  overscan = 5,
  containerRef,
  onRangeChange,
}: UseVirtualScrollOptions): UseVirtualScrollReturn {
  const [, setTick] = useState(0);
  const scrollTopRef = useRef(0);
  const viewportHeightRef = useRef(0);
  const rafId = useRef<number | null>(null);
  const lastFiredRef = useRef<string>('');
  const onRangeChangeRef = useRef(onRangeChange);
  onRangeChangeRef.current = onRangeChange;
  const prevTotalItemsRef = useRef(totalItems);

  const scrollTop = scrollTopRef.current;
  const [viewportHeight, setViewportHeight] = useState(0);

  // ResizeObserver: detect container height changes and re-trigger render
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const h = entry.contentBoxSize?.[0]?.blockSize ?? entry.contentRect.height;
        if (h !== viewportHeightRef.current) {
          viewportHeightRef.current = h;
          setViewportHeight(h);
        }
      }
    });
    // Read initial height
    const initialH = container.clientHeight;
    if (initialH !== viewportHeightRef.current) {
      viewportHeightRef.current = initialH;
      setViewportHeight(initialH);
    }
    observer.observe(container);
    return () => observer.disconnect();
  }, [containerRef]);

  const needsScaling = totalItems * itemHeight > SCROLL_SPACE;
  const effectiveItemHeight = needsScaling
    ? SCROLL_SPACE / totalItems
    : itemHeight;

  const spacerHeight = needsScaling ? SCROLL_SPACE : totalItems * itemHeight;
  const maxScrollTop = Math.max(spacerHeight - viewportHeight, 0);
  // 钳制 scrollTop 到有效范围，防止浏览器滚动越界导致内容偏移
  const clampedScrollTop = Math.max(0, Math.min(scrollTop, maxScrollTop));
  const rawStartIndex = Math.floor(clampedScrollTop / effectiveItemHeight);
  const visibleCount = Math.ceil(viewportHeight / itemHeight) + overscan;

  const atBottom = totalItems > 0 && maxScrollTop > 0 && clampedScrollTop >= maxScrollTop - 1;

  // 当 totalItems 减小（文件被删除）且之前在底部时，强制保持在底部。
  // 否则 maxScrollTop 减小后 scrollTop 被钳制，atBottom 失效，startIndex 跳变导致内容上移。
  const totalShrank = totalItems < prevTotalItemsRef.current;
  prevTotalItemsRef.current = totalItems;

  let startIndex: number;
  if (totalItems > 0) {
    if (atBottom || (totalShrank && clampedScrollTop >= maxScrollTop - viewportHeight)) {
      startIndex = Math.max(totalItems - visibleCount, 0);
    } else {
      startIndex = Math.min(rawStartIndex, Math.max(totalItems - visibleCount, 0));
    }
  } else {
    startIndex = 0;
  }
  const endIndex = Math.min(startIndex + visibleCount, totalItems);
  // 取整避免 sub-pixel 定位导致行边缘抗锯齿重影
  const offsetY = Math.round(clampedScrollTop);

  const handleScroll = useCallback(() => {
    if (rafId.current !== null) {
      cancelAnimationFrame(rafId.current);
    }
    rafId.current = requestAnimationFrame(() => {
      if (containerRef.current) {
        scrollTopRef.current = containerRef.current.scrollTop;
        setTick(t => t + 1);
      }
      rafId.current = null;
    });
  }, [containerRef]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const handleWheel = (e: WheelEvent) => {
      e.preventDefault();
      container.scrollTop += e.deltaY * 0.25;
    };

    container.addEventListener('scroll', handleScroll, { passive: true });
    container.addEventListener('wheel', handleWheel, { passive: false });
    return () => {
      container.removeEventListener('scroll', handleScroll);
      container.removeEventListener('wheel', handleWheel);
      if (rafId.current !== null) {
        cancelAnimationFrame(rafId.current);
      }
    };
  }, [containerRef, handleScroll]);

  useEffect(() => {
    if (totalItems === 0) return;
    const key = `${startIndex}-${endIndex}`;
    if (key !== lastFiredRef.current) {
      lastFiredRef.current = key;
      onRangeChangeRef.current?.(startIndex, endIndex);
    }
  }, [startIndex, endIndex, totalItems]);

  const scrollToIndex = useCallback((index: number) => {
    if (containerRef.current) {
      containerRef.current.scrollTop = index * itemHeight;
      scrollTopRef.current = containerRef.current.scrollTop;
      setTick(t => t + 1);
    }
  }, [containerRef, itemHeight]);

  const resetScroll = useCallback(() => {
    if (containerRef.current) {
      containerRef.current.scrollTop = 0;
    }
    scrollTopRef.current = 0;
    setTick(t => t + 1);
  }, [containerRef]);

  return {
    startIndex,
    endIndex,
    offsetY,
    spacerHeight,
    visibleItems: endIndex - startIndex,
    scrollToIndex,
    resetScroll,
  };
}
