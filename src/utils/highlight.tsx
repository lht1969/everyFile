import React from 'react';

/**
 * 从搜索条件中提取用于高亮的纯文本 token。
 * 过滤掉 filter 语法（如 size:>1GB、path:Downloads、dm:2026、regex:...），
 * 仅保留用户输入的关键词。
 */
function extractHighlightTokens(query: string): string[] {
  const tokens: string[] = [];
  // 匹配 filter 语法：key:value（key 为已知前缀则跳过 value）
  const filterPattern = /^(size|path|date\w*|dm|dc|da|regex)\s*:/i;
  // 按空格分隔，支持多关键词
  const parts = query.split(/\s+/);
  for (const part of parts) {
    if (!part) continue;
    if (filterPattern.test(part)) continue;
    // 去掉通配符 * ? []，保留纯文本用于匹配
    const plain = part.replace(/[*?\[\]]/g, '');
    if (plain) tokens.push(plain);
  }
  return tokens;
}

/**
 * 对文本进行搜索高亮：将匹配的字符加粗显示。
 * 返回 React 元素数组。
 *
 * @param text - 要显示的原始文本
 * @param query - 搜索条件
 */
export function highlightMatch(text: string, query: string): React.ReactNode {
  if (!query.trim()) return text;

  const tokens = extractHighlightTokens(query);
  if (tokens.length === 0) return text;

  // 构建匹配区间列表
  const textLower = text.toLowerCase();
  const ranges: [number, number][] = [];

  for (const token of tokens) {
    const tokenLower = token.toLowerCase();
    let idx = 0;
    while (idx < textLower.length) {
      const pos = textLower.indexOf(tokenLower, idx);
      if (pos === -1) break;
      ranges.push([pos, pos + tokenLower.length]);
      idx = pos + 1;
    }
  }

  if (ranges.length === 0) return text;

  // 合并重叠区间
  ranges.sort((a, b) => a[0] - b[0]);
  const merged: [number, number][] = [ranges[0]];
  for (let i = 1; i < ranges.length; i++) {
    const last = merged[merged.length - 1];
    if (ranges[i][0] <= last[1]) {
      last[1] = Math.max(last[1], ranges[i][1]);
    } else {
      merged.push(ranges[i]);
    }
  }

  // 按区间切分文本
  const parts: React.ReactNode[] = [];
  let cursor = 0;
  for (const [start, end] of merged) {
    if (cursor < start) {
      parts.push(text.slice(cursor, start));
    }
    parts.push(<strong key={start}>{text.slice(start, end)}</strong>);
    cursor = end;
  }
  if (cursor < text.length) {
    parts.push(text.slice(cursor));
  }

  return parts;
}
