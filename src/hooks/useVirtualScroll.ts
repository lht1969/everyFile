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
  // visibleCount: 数据请求 / endIndex 范围，基于 effectiveItemHeight（滚动空间行高），
  // 确保在缩放模式下 endIndex 能覆盖到 totalItems，否则尾部行会被截断。
  const visibleCount = Math.ceil(viewportHeight / effectiveItemHeight) + overscan;
  // visibleInView: 视口内实际能容纳的渲染行数（不含 overscan），基于 itemHeight
  // （实际渲染行高 28px），用于底部 startIndex 钳制，避免溢出视口。
  const visibleInView = Math.ceil(viewportHeight / itemHeight);

  // 当 totalItems 减小（文件被删除）且之前在底部时，强制保持在底部。
  // 否则 maxScrollTop 减小后 scrollTop 被钳制，startIndex 跳变导致内容上移。
  const totalShrank = totalItems < prevTotalItemsRef.current;
  prevTotalItemsRef.current = totalItems;

  const atBottom = totalItems > 0 && maxScrollTop > 0 && clampedScrollTop >= maxScrollTop - 1;

  let startIndex: number;
  if (totalItems > 0) {
    // 只有当内容超过视口高度时才需要底部钳制。
    // 否则当 totalItems 很小（如 7 行）且 viewportHeight 较大（如 560px）时，
    // spacerHeight(196) < viewportHeight(560)，maxScrollTop=0，
    // totalShrank && clampedScrollTop(0) >= maxScrollTop(0) - viewportHeight(560) = -560
    // 条件成立，startIndex = totalItems - visibleInView 可能为负数，
    // 虽然会被 max(0) 钳制，但 offsetY 若基于 spacerHeight - visibleInView * itemHeight 会得到负数，
    // 导致 virtual-content 被移出可视区域。此处保留 needsBottomClamp 仅用于 startIndex 计算，
    // offsetY 统一使用 clampedScrollTop，避免循环。
    const needsBottomClamp = spacerHeight > viewportHeight;
    if (needsBottomClamp && (atBottom || (totalShrank && clampedScrollTop >= maxScrollTop - viewportHeight))) {
      // 底部时直接用 visibleInView 钳制，确保最后一行落在视口内。
      // rawStartIndex 基于 effectiveItemHeight 可能远小于 totalItems - visibleInView（缩放模式），
      // 不钳制的话 startIndex 过小，末尾行会溢出视口。
      startIndex = Math.max(totalItems - visibleInView, 0);
    } else {
      startIndex = Math.min(rawStartIndex, Math.max(totalItems - visibleInView, 0));
    }
  } else {
    startIndex = 0;
  }
  const endIndex = Math.min(startIndex + visibleCount, totalItems);
  // 使用 clampedScrollTop 作为 offsetY，确保 virtual-content 位置与浏览器 scrollTop 同步。
  // 原因：之前 bottomClamped 时使用 spacerHeight - visibleInView * itemHeight，
  // 当 viewportHeight 不是 itemHeight 整数倍或 totalItems 频繁变化时，
  // offsetY 与 clampedScrollTop 的差值会不断变化，触发浏览器 scroll 调整 →
  // handleScroll → setTick → 重渲染 → onRangeChange → fetch 的循环，
  // 表现为最后一行持续上移/下移，CPU 占用高，最终内容移出窗口。
  // clampedScrollTop 由浏览器真实滚动位置决定，是最稳定的基准。
  // 底部时 clampedScrollTop ≈ maxScrollTop，最后一行自然位于视口底部。
  const offsetY = Math.round(clampedScrollTop);

  // 在底部且 maxScrollTop 变化时，强制将 scrollTop 对齐到 maxScrollTop。
  // 原因：totalItems 因 USN 增量更新变化时，浏览器会自动调整 scrollTop，
  // 但调整后的值可能与 maxScrollTop 有偏差（如 sub-pixel 或延迟），
  // 导致 clampedScrollTop 变化 → offsetY 变化 → 内容上下移动；
  // 极端情况下 scrollTop 未及时调整，内容会完全移出窗口。
  // 此处主动同步，确保底部时 scrollTop 始终等于 maxScrollTop，内容位置稳定。
  useEffect(() => {
    const container = containerRef.current;
    if (!container || !atBottom || maxScrollTop <= 0) return;
    if (Math.abs(container.scrollTop - maxScrollTop) > 0.5) {
      scrollTopRef.current = maxScrollTop;
      container.scrollTop = maxScrollTop;
    }
  }, [atBottom, maxScrollTop]);

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

  // 注意：依赖数组不包含 totalItems。
  // 原因：records-refresh 事件会调用 setTotalCount(total - 1)，
  // 若 totalItems 在依赖数组中，则会重新触发 onRangeChange →
  // handleVisibleRangeChange → atBottom 时 ++fetchCounterRef.current
  // 使 records-refresh 自己触发的 fetchRecordsRange 失效，invoke 返回后
  // 结果被丢弃；期间 results 仍是旧的，但 startIndex 已基于新的 totalItems
  // 重新计算（底部钳制 startIndex = totalItems - visibleInView），不匹配，
  // 渲染占位符形成 4 秒空白窗口。
  // startIndex/endIndex 的变化本身已经能触发 useEffect，无需 totalItems。
  useEffect(() => {
    if (totalItems === 0) return;
    const key = `${startIndex}-${endIndex}`;
    if (key !== lastFiredRef.current) {
      lastFiredRef.current = key;
      onRangeChangeRef.current?.(startIndex, endIndex);
    }
  }, [startIndex, endIndex]);

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
