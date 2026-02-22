export const SEARCH_DEBOUNCE_MS = 200;

const PRESERVE_VIEWPORT_BUFFER_ROWS = 120;
const MAX_PRESERVE_FETCH_LIMIT = 2500;

export function extractLeafQuery(query) {
  const q = (query || '').trim();
  const slash = q.lastIndexOf('/');
  const backslash = q.lastIndexOf('\\');
  const splitAt = Math.max(slash, backslash);
  if (splitAt < 0) {
    return q;
  }
  return q.slice(splitAt + 1);
}

export function computeSearchFetchLimit({
  preserveScroll,
  pageSize,
  loadedCount,
  viewportStart,
  viewportRows
}) {
  if (!preserveScroll) {
    return pageSize;
  }

  const minForViewport = viewportStart + viewportRows + PRESERVE_VIEWPORT_BUFFER_ROWS;
  if (minForViewport > MAX_PRESERVE_FETCH_LIMIT) {
    return null;
  }

  const target = Math.max(pageSize, minForViewport, Math.min(loadedCount, MAX_PRESERVE_FETCH_LIMIT));
  return Math.min(target, MAX_PRESERVE_FETCH_LIMIT);
}

export function shouldApplyPreserveResults(entriesLength, viewportStart, viewportRows) {
  return entriesLength >= (viewportStart + viewportRows + 1);
}