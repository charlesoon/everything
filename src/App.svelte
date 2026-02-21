<script>
  import { onDestroy, onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { startDrag } from '@crabnebula/tauri-plugin-drag';
  import 'overlayscrollbars/overlayscrollbars.css';
  import { OverlayScrollbars } from 'overlayscrollbars';

  let osInstance = null;
  let osViewport = null;
  let scrollCleanup = null;
  function getScrollEl() {
    return osViewport || tableContainer;
  }

  const rowHeight = 28;
  const PAGE_SIZE = 500;
  let homePrefix = '';
  const COL_WIDTHS_KEY = 'everything-col-widths-v2';
  const columnKeys = ['name', 'path', 'size', 'modified'];
  const minColumnWidth = {
    name: 180,
    path: 240,
    size: 75,
    modified: 120
  };
  const defaultColumnRatios = {
    name: 0.3,
    path: 0.45,
    size: 0.1
  };
  const DEBUG_STARTUP = false;

  let query = '';
  let results = [];
  let totalResults = 0;
  let totalResultsKnown = false;

  let dbLatencyMs = null;
  let dbLastQuery = '';
  let searchModeLabel = '';
  let selectedIndices = new Set();
  let selectionAnchor = -1;
  let lastSelectedIndex = -1;
  let editing = {
    active: false,
    path: '',
    index: -1,
    draftName: ''
  };

  let indexStatus = {
    state: 'Indexing',
    entriesCount: 0,
    lastUpdated: null,
    permissionErrors: 0,
    message: null,
    isCatchup: false
  };

  let indexingStartTime = null;
  let indexingElapsed = '';
  let indexingFinishedAt = '';

  let sortBy = 'name';
  let sortDir = 'asc';

  let hasMore = true;
  let loadingMore = false;
  let searchGeneration = 0;
  let scheduleGeneration = 0;
  let searchPending = false;

  let scanned = 0;
  let indexed = 0;
  let currentPath = '';

  let searchInputEl;
  let renameInputEl;
  let tableAreaEl;
  let tableContainer;
  let scrollTop = 0;
  let headerScrollLeft = 0;
  let viewportHeight = 520;
  let lastTableWidth = 0;
  let resizingColumn = '';
  let resizeCleanup = null;
  let colWidths = {
    name: 250,
    path: 350,
    size: 75,
    modified: 120
  };

  let contextMenu = {
    visible: false,
    x: 0,
    y: 0
  };

  let platform = '';
  let toast = '';
  let searchTimer;
  let lastSearchFiredAt = 0;
  let toastTimer;
  let statusRefreshTimer;
  let elapsedTimer;
  let statusRefreshInFlight = false;
  let resetInFlight = false;
  let lastReadyCount = 0;

  const iconCache = new Map();
  const iconLoading = new Set();

  let highlightCache = new Map();
  let highlightCacheQuery = '';

  function startupLog(msg) {
    if (!DEBUG_STARTUP) {
      return;
    }
    void invoke('frontend_log', { msg }).catch(() => {});
  }

  const folderFallbackIcon =
    "data:image/svg+xml;utf8," +
    encodeURIComponent(
      `<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><rect x='1' y='4' width='14' height='10' rx='2' fill='#d9b34f'/><rect x='1' y='2' width='6' height='4' rx='1' fill='#e6ca7a'/></svg>`
    );
  const fileFallbackIcon =
    "data:image/svg+xml;utf8," +
    encodeURIComponent(
      `<svg xmlns='http://www.w3.org/2000/svg' width='16' height='16'><rect x='3' y='1' width='10' height='14' rx='1.5' fill='#b4bec9'/><rect x='5' y='5' width='6' height='1.2' fill='#8d98a5'/><rect x='5' y='8' width='6' height='1.2' fill='#8d98a5'/></svg>`
    );

  $: totalHeight = results.length * rowHeight;
  $: startIndex = Math.max(0, Math.floor(scrollTop / rowHeight) - 10);
  $: visibleCount = Math.ceil(viewportHeight / rowHeight) + 20;
  $: endIndex = Math.min(results.length, startIndex + visibleCount);
  $: visibleRows = results.slice(startIndex, endIndex);
  $: translateY = startIndex * rowHeight;

  $: tableMinWidth = colWidths.name + colWidths.path + colWidths.size + colWidths.modified;
  $: tableGridStyle = `--col-name:${colWidths.name}px;--col-path:${colWidths.path}px;--col-size:${colWidths.size}px;--col-modified:${colWidths.modified}px;--table-min-width:${tableMinWidth}px;--header-offset:${-headerScrollLeft}px;`;

  $: {
    for (const entry of visibleRows) {
      void ensureIcon(entry);
    }
  }

  function showToast(message) {
    toast = message;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      toast = '';
    }, 3000);
  }

  function bytesToBase64(bytes) {
    let binary = '';
    const chunk = 0x8000;
    for (let i = 0; i < bytes.length; i += chunk) {
      const part = bytes.subarray(i, i + chunk);
      binary += String.fromCharCode(...part);
    }
    return btoa(binary);
  }

  function highlightSegments(name, q) {
    q = (q || '').trim();
    if (!q) return [{ text: name, hl: false }];

    // Reset cache when query changes
    if (q !== highlightCacheQuery) {
      highlightCache = new Map();
      highlightCacheQuery = q;
    }

    // Check cache
    const cached = highlightCache.get(name);
    if (cached) return cached;

    if (q.includes('/')) q = q.slice(q.lastIndexOf('/') + 1);
    q = q.replace(/[*?]/g, '');
    const terms = q.split(/\s+/).filter(Boolean);
    if (terms.length === 0) return [{ text: name, hl: false }];

    const lower = name.toLowerCase();
    const marks = new Uint8Array(name.length);
    for (const t of terms) {
      const tl = t.toLowerCase();
      let idx = lower.indexOf(tl);
      while (idx !== -1) {
        for (let i = idx; i < idx + tl.length; i++) marks[i] = 1;
        idx = lower.indexOf(tl, idx + 1);
      }
    }

    const segs = [];
    let i = 0;
    while (i < name.length) {
      const m = marks[i];
      let j = i + 1;
      while (j < name.length && marks[j] === m) j++;
      segs.push({ text: name.slice(i, j), hl: m === 1 });
      i = j;
    }

    highlightCache.set(name, segs);
    return segs;
  }

  const perFileIconExts = new Set(['exe', 'lnk', 'ico', 'url', 'scr', 'appx']);

  function iconKey(entry) {
    if (entry.isDir) return '__folder__';
    const ext = (entry.ext || '').toLowerCase();
    if (perFileIconExts.has(ext)) return entry.path;
    return ext || '__file__';
  }

  function iconFor(entry) {
    return iconCache.get(iconKey(entry)) || (entry.isDir ? folderFallbackIcon : fileFallbackIcon);
  }

  async function ensureIcon(entry) {
    const key = iconKey(entry);
    if (iconCache.has(key) || iconLoading.has(key)) {
      return;
    }

    iconLoading.add(key);
    try {
      const bytes = await invoke('get_file_icon', {
        path: entry.path,
        ext: entry.isDir ? 'folder' : entry.ext || '',
      });
      if (Array.isArray(bytes) && bytes.length > 0) {
        const image = `data:image/png;base64,${bytesToBase64(Uint8Array.from(bytes))}`;
        iconCache.set(key, image);
      } else {
        iconCache.set(key, entry.isDir ? folderFallbackIcon : fileFallbackIcon);
      }
    } catch {
      iconCache.set(key, entry.isDir ? folderFallbackIcon : fileFallbackIcon);
    } finally {
      iconLoading.delete(key);
    }
  }

  function syncColumnWidthsToContainer() {
    const width = tableContainer ? getScrollEl().clientWidth : tableAreaEl?.clientWidth;
    if (!width || lastTableWidth !== 0) {
      return;
    }

    const saved = loadColWidths();
    if (saved) {
      const total = columnKeys.reduce((s, k) => s + saved[k], 0);
      if (Math.abs(total - width) > 2) {
        const scale = (width - saved.modified) / (total - saved.modified);
        let scaledName = Math.max(minColumnWidth.name, Math.round(saved.name * scale));
        let scaledSize = Math.max(minColumnWidth.size, Math.round(saved.size * scale));
        let rawPath = width - saved.modified - scaledName - scaledSize;
        if (rawPath < minColumnWidth.path) {
          const shortfall = minColumnWidth.path - rawPath;
          const nameReduction = Math.min(shortfall, scaledName - minColumnWidth.name);
          scaledName -= nameReduction;
          const remaining = shortfall - nameReduction;
          if (remaining > 0) {
            scaledSize -= Math.min(remaining, scaledSize - minColumnWidth.size);
          }
          rawPath = width - saved.modified - scaledName - scaledSize;
        }
        const scaledPath = Math.max(minColumnWidth.path, rawPath);
        colWidths = { name: scaledName, path: scaledPath, size: scaledSize, modified: saved.modified };
      } else {
        colWidths = saved;
      }
    } else {
      const fixedModified = minColumnWidth.modified;
      const rest = width - fixedModified;
      const ratioSum = defaultColumnRatios.name + defaultColumnRatios.path + defaultColumnRatios.size;
      colWidths = {
        name: Math.max(minColumnWidth.name, Math.round(rest * defaultColumnRatios.name / ratioSum)),
        path: Math.max(minColumnWidth.path, Math.round(rest * defaultColumnRatios.path / ratioSum)),
        size: Math.max(minColumnWidth.size, Math.round(rest * defaultColumnRatios.size / ratioSum)),
        modified: fixedModified
      };
    }
    lastTableWidth = width;
  }

  function loadColWidths() {
    try {
      const raw = localStorage.getItem(COL_WIDTHS_KEY);
      if (!raw) return null;
      const parsed = JSON.parse(raw);
      for (const key of columnKeys) {
        if (typeof parsed[key] !== 'number' || parsed[key] < (minColumnWidth[key] || 0)) return null;
      }
      return parsed;
    } catch {
      return null;
    }
  }

  function saveColWidths() {
    localStorage.setItem(COL_WIDTHS_KEY, JSON.stringify(colWidths));
  }

  function updateViewportHeight() {
    viewportHeight = tableContainer ? getScrollEl().clientHeight : 520;
    syncColumnWidthsToContainer();
  }

  function startColumnResize(event, leftKey) {
    event.preventDefault();
    event.stopPropagation();

    const index = columnKeys.indexOf(leftKey);
    if (index < 0 || index >= columnKeys.length - 1) {
      return;
    }

    const startX = event.clientX;
    const startLeft = colWidths[leftKey];
    const rightKey = columnKeys[index + 1];
    const startRight = colWidths[rightKey];

    resizeCleanup?.();
    resizingColumn = leftKey;

    const onMove = (moveEvent) => {
      const delta = Math.round(moveEvent.clientX - startX);
      if (leftKey === 'name') {
        // Two-column trade: name and path swap space, each respects its minimum
        const nextLeft = Math.max(
          minColumnWidth[leftKey],
          Math.min(startLeft + delta, startLeft + startRight - minColumnWidth[rightKey])
        );
        const nextRight = startLeft + startRight - nextLeft;
        colWidths = { ...colWidths, [leftKey]: nextLeft, [rightKey]: nextRight };
      } else {
        // path splitter: path grows unconstrained (allows horizontal scroll on right drag)
        const nextLeft = Math.max(minColumnWidth[leftKey], startLeft + delta);
        colWidths = { ...colWidths, [leftKey]: nextLeft };
      }
    };

    const onUp = () => {
      cleanup();
    };

    const cleanup = () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
      resizingColumn = '';
      saveColWidths();
      if (resizeCleanup === cleanup) {
        resizeCleanup = null;
      }
    };

    resizeCleanup = cleanup;
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
  }

  function selectedPaths() {
    const sorted = [...selectedIndices].sort((a, b) => a - b);
    return sorted.map((idx) => results[idx]).filter(Boolean).map((entry) => entry.path);
  }

  function clearSelection() {
    selectedIndices = new Set();
    selectionAnchor = -1;
    lastSelectedIndex = -1;
  }

  function selectSingle(index) {
    selectedIndices = new Set([index]);
    selectionAnchor = index;
    lastSelectedIndex = index;
  }

  function selectToggle(index) {
    const next = new Set(selectedIndices);
    if (next.has(index)) {
      next.delete(index);
    } else {
      next.add(index);
    }
    selectedIndices = next;
    selectionAnchor = index;
    lastSelectedIndex = index;
  }

  function selectRange(index) {
    const anchor = selectionAnchor >= 0 ? selectionAnchor : index;
    const [from, to] = [Math.min(anchor, index), Math.max(anchor, index)];
    const next = new Set();
    for (let i = from; i <= to; i += 1) {
      next.add(i);
    }
    selectedIndices = next;
    lastSelectedIndex = index;
  }

  function primaryEntry() {
    const idx = lastSelectedIndex >= 0 ? lastSelectedIndex : -1;
    return idx >= 0 ? results[idx] : null;
  }

  async function refreshStatus() {
    if (statusRefreshInFlight || resetInFlight) {
      startupLog('[startup/fe] refreshStatus skipped (in-flight)');
      return;
    }
    statusRefreshInFlight = true;
    const startedAt = performance.now();
    startupLog('[startup/fe] refreshStatus start');
    try {
      const status = await invoke('get_index_status');
      const prevState = indexStatus.state;
      indexStatus = {
        state: status.state,
        entriesCount: status.entriesCount,
        lastUpdated: status.lastUpdated,
        permissionErrors: status.permissionErrors ?? 0,
        message: status.message
      };
      if (status.state === 'Indexing' && prevState !== 'Indexing') {
        startElapsedTimer();
      } else if (status.state !== 'Indexing' && prevState === 'Indexing') {
        stopElapsedTimer();
      }
      if (typeof status.scanned === 'number') {
        scanned = status.scanned;
      }
      if (typeof status.indexed === 'number') {
        indexed = status.indexed;
      }
      if (typeof status.currentPath === 'string') {
        currentPath = status.currentPath;
      }
      if (status.state === 'Ready' && prevState !== 'Ready') {
        scheduleSearch(true);
      }
      startupLog(
        `[startup/fe] refreshStatus done in ${Math.round(performance.now() - startedAt)}ms `
        + `(state=${status.state}, entries=${status.entriesCount}, scanned=${status.scanned ?? 'n/a'}, `
        + `indexed=${status.indexed ?? 'n/a'})`
      );
    } catch (err) {
      startupLog(
        `[startup/fe] refreshStatus failed in ${Math.round(performance.now() - startedAt)}ms: ${String(err)}`
      );
      showToast(`Failed to get status: ${String(err)}`);
    } finally {
      statusRefreshInFlight = false;
    }
  }

  function scheduleSearch(preserveScroll = false) {
    // Use a separate counter for debounce cancellation so that scheduling
    // a search does not invalidate in-flight loadMore calls (which check
    // searchGeneration). Only runSearch bumps searchGeneration.
    scheduleGeneration += 1;
    const scheduledGen = scheduleGeneration;
    clearTimeout(searchTimer);

    const now = performance.now();
    if (now - lastSearchFiredAt >= 200) {
      // Leading edge: fire immediately if enough time has passed
      lastSearchFiredAt = now;
      void runSearch(preserveScroll);
    } else {
      // Trailing edge: debounce
      searchTimer = setTimeout(() => {
        if (scheduledGen !== scheduleGeneration) {
          return;
        }
        lastSearchFiredAt = performance.now();
        void runSearch(preserveScroll);
      }, 200);
    }
  }

  async function runSearch(preserveScroll = false) {
    searchGeneration += 1;
    const gen = searchGeneration;
    searchPending = true;
    const searchQuery = query;
    const searchSortBy = sortBy;
    const searchSortDir = sortDir;
    const startedAt = performance.now();
    // When preserving scroll, reload at least as many results as currently
    // loaded so totalHeight stays stable and scrollTop isn't clamped.
    const fetchLimit = preserveScroll
      ? Math.max(PAGE_SIZE, results.length)
      : PAGE_SIZE;
    try {
      const keepPaths = new Set(selectedPaths());
      const next = await invoke('search', {
        query: searchQuery,
        limit: fetchLimit,
        offset: 0,
        sortBy: searchSortBy,
        sortDir: searchSortDir
      });

      if (gen !== searchGeneration) return;

      dbLatencyMs = Math.round(performance.now() - startedAt);
      dbLastQuery = searchQuery;
      const entries = Array.isArray(next.entries) ? next.entries : [];
      searchModeLabel = next.modeLabel || '';

      // During the await, loadMore may have grown results beyond fetchLimit.
      // Replacing a larger result set with a smaller one would shrink
      // totalHeight and cause the browser to clamp scrollTop → scroll jump.
      // Skip this update; the existing results are still valid (same query/sort).
      if (preserveScroll && entries.length < results.length) return;

      if (preserveScroll && tableContainer) scrollTop = getScrollEl().scrollTop;
      results = entries;
      if (next.totalCount > 0) {
        totalResults = next.totalCount;
        totalResultsKnown = true;
      } else {
        totalResults = entries.length;
        totalResultsKnown = false;
      }
      hasMore = totalResultsKnown ? results.length < totalResults : entries.length >= fetchLimit;
      if (!preserveScroll && tableContainer) getScrollEl().scrollTop = 0;

      const restored = new Set();
      for (let i = 0; i < results.length; i += 1) {
        if (keepPaths.has(results[i].path)) {
          restored.add(i);
        }
      }
      selectedIndices = restored;

      updateViewportHeight();
    } catch (err) {
      if (gen !== searchGeneration) return;
      showToast(`Search failed: ${String(err)}`);
    } finally {
      searchPending = false;
    }
  }

  async function loadMore() {
    if (!hasMore || loadingMore || searchPending) return;
    const gen = searchGeneration;
    loadingMore = true;
    try {
      const batch = await invoke('search', {
        query,
        limit: PAGE_SIZE,
        offset: results.length,
        sortBy: sortBy,
        sortDir: sortDir
      });
      if (gen !== searchGeneration) return;
      const arr = Array.isArray(batch.entries) ? batch.entries : [];
      if (arr.length > 0) {
        if (tableContainer) scrollTop = getScrollEl().scrollTop;
        results = [...results, ...arr];
      }
      hasMore = totalResultsKnown ? results.length < totalResults : arr.length >= PAGE_SIZE;
    } catch (err) {
      showToast(`Failed to load more: ${String(err)}`);
    } finally {
      loadingMore = false;
    }
  }

  function moveSelection(delta, withRange = false) {
    if (results.length === 0) {
      return;
    }

    const current = lastSelectedIndex >= 0 ? lastSelectedIndex : 0;
    const next = Math.max(0, Math.min(results.length - 1, current + delta));

    if (withRange) {
      selectRange(next);
    } else {
      selectSingle(next);
    }

    const top = next * rowHeight;
    const bottom = top + rowHeight;
    if (top < scrollTop) {
      if (tableContainer) getScrollEl().scrollTop = top;
    } else if (bottom > scrollTop + viewportHeight) {
      if (tableContainer) getScrollEl().scrollTop = bottom - viewportHeight;
    }
  }

  function handleHeaderSort(column) {
    if (sortBy === column) {
      sortDir = sortDir === 'asc' ? 'desc' : 'asc';
    } else {
      sortBy = column;
      sortDir = 'asc';
    }
    if (tableContainer) getScrollEl().scrollTop = 0;
    void runSearch();
  }

  function handleRowClick(event, index) {
    contextMenu.visible = false;

    if (event.shiftKey) {
      selectRange(index);
      return;
    }

    if (event.metaKey || event.ctrlKey) {
      selectToggle(index);
      return;
    }

    selectSingle(index);
  }

  function handleRowContextMenu(event, index) {
    event.preventDefault();

    if (!selectedIndices.has(index)) {
      selectSingle(index);
    }

    if (platform === 'windows' || platform === 'macos') {
      contextMenu.visible = false;
      const paths = selectedPaths();
      if (paths.length > 0) {
        invoke('show_context_menu', {
          paths,
          x: event.clientX,
          y: event.clientY,
          singleSelection: selectedIndices.size === 1
        });
      }
      return;
    }

    contextMenu = {
      visible: true,
      x: event.clientX,
      y: event.clientY
    };
  }

  function handleRowA11yKeydown(event, index) {
    if (event.key === ' ' || event.key === 'Enter') {
      event.preventDefault();
      handleRowClick(event, index);
    }
  }

  const DRAG_THRESHOLD = 5;
  let dragCleanup = null;
  const dragImgCache = new Map();

  function dragPathsForIndex(index) {
    const entry = results[index];
    if (!entry) {
      return [];
    }

    if (selectedIndices.has(index) && selectedIndices.size > 0) {
      return selectedPaths();
    }

    return [entry.path];
  }

  function getDragIconEl(entry) {
    const src = iconFor(entry);
    let img = dragImgCache.get(src);
    if (!img) {
      img = new Image();
      img.src = src;
      dragImgCache.set(src, img);
    }
    return img.complete && img.naturalWidth > 0 ? img : null;
  }

  function fillRoundRect(ctx, x, y, w, h, r) {
    ctx.beginPath();
    ctx.moveTo(x + r, y);
    ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r);
    ctx.arcTo(x, y + h, x, y, r);
    ctx.arcTo(x, y, x + w, y, r);
    ctx.closePath();
    ctx.fill();
  }

  function buildDragPreview(index) {
    const entry = results[index];
    if (!entry) return null;

    const count = selectedIndices.has(index) ? selectedIndices.size : 1;
    const isDark = window.matchMedia('(prefers-color-scheme: dark)').matches;

    const w = 260;
    const h = 28;
    const stackGap = count > 1 ? 4 : 0;

    const canvas = document.createElement('canvas');
    canvas.width = w;
    canvas.height = h + stackGap;
    const ctx = canvas.getContext('2d');

    if (count > 1) {
      ctx.globalAlpha = 0.35;
      ctx.fillStyle = isDark ? '#5a6070' : '#b0c4de';
      fillRoundRect(ctx, 3, 0, w - 3, h, 5);
      ctx.globalAlpha = 1.0;
    }

    const ry = stackGap;
    ctx.fillStyle = isDark ? 'rgba(58, 64, 74, 0.94)' : 'rgba(214, 231, 255, 0.94)';
    fillRoundRect(ctx, 0, ry, w, h, 5);

    ctx.strokeStyle = isDark ? 'rgba(255,255,255,0.1)' : 'rgba(0,0,0,0.1)';
    ctx.lineWidth = 0.5;
    ctx.beginPath();
    ctx.moveTo(5, ry);
    ctx.arcTo(w, ry, w, ry + h, 5);
    ctx.arcTo(w, ry + h, 0, ry + h, 5);
    ctx.arcTo(0, ry + h, 0, ry, 5);
    ctx.arcTo(0, ry, w, ry, 5);
    ctx.closePath();
    ctx.stroke();

    const iconEl = getDragIconEl(entry);
    if (iconEl) {
      ctx.drawImage(iconEl, 6, ry + 6, 16, 16);
    } else {
      ctx.fillStyle = entry.isDir ? '#d9b34f' : '#8d98a5';
      fillRoundRect(ctx, 6, ry + 6, 16, 16, 2);
    }

    ctx.fillStyle = isDark ? '#e6e6e8' : '#1a1a1a';
    ctx.font = '12px -apple-system, "SF Pro Text", system-ui, sans-serif';
    ctx.textBaseline = 'middle';

    let nameMaxW = w - 34;
    if (count > 1) nameMaxW -= 34;

    let name = entry.name;
    if (ctx.measureText(name).width > nameMaxW) {
      while (name.length > 4 && ctx.measureText(name + '\u2026').width > nameMaxW) {
        name = name.slice(0, -1);
      }
      name += '\u2026';
    }
    ctx.fillText(name, 28, ry + h / 2 + 1);

    if (count > 1) {
      const badge = String(count);
      ctx.font = 'bold 10px -apple-system, system-ui, sans-serif';
      const tw = ctx.measureText(badge).width;
      const bw = Math.max(20, tw + 10);
      const bx = w - bw - 6;
      const by = ry + (h - 18) / 2;

      ctx.fillStyle = isDark ? '#5b8bd9' : '#007aff';
      fillRoundRect(ctx, bx, by, bw, 18, 9);

      ctx.fillStyle = '#ffffff';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText(badge, bx + bw / 2, by + 9);
    }

    return canvas.toDataURL('image/png');
  }

  function handleRowMouseDown(event, index) {
    if (event.button !== 0) return;
    if (event.metaKey || event.ctrlKey || event.shiftKey) return;
    if (editing.active) return;

    const startX = event.clientX;
    const startY = event.clientY;
    let dragging = false;

    dragCleanup?.();

    const onMove = (e) => {
      if (dragging) return;
      const dx = e.clientX - startX;
      const dy = e.clientY - startY;
      if (dx * dx + dy * dy < DRAG_THRESHOLD * DRAG_THRESHOLD) return;

      dragging = true;
      cleanup();

      if (!selectedIndices.has(index)) {
        selectSingle(index);
      }

      const paths = dragPathsForIndex(index);
      if (paths.length > 0) {
        const icon = buildDragPreview(index);
        startDrag({ item: paths, icon }).catch(() => {});
      }
    };

    const onUp = () => cleanup();

    const cleanup = () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      if (dragCleanup === cleanup) dragCleanup = null;
    };

    dragCleanup = cleanup;
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
  }

  async function handleRowDoubleClick(index) {
    if (!selectedIndices.has(index) || selectedIndices.size > 1) {
      selectSingle(index);
      await tick();
    }
    await invoke('open', { paths: [results[index].path] }).catch((err) => {
      showToast(`Failed to open: ${String(err)}`);
    });
  }

  function closeContextMenu() {
    contextMenu = {
      ...contextMenu,
      visible: false
    };
  }

  async function openSelected() {
    const paths = selectedPaths();
    if (paths.length === 0) {
      return;
    }

    try {
      await invoke('open', { paths });
    } catch (err) {
      showToast(`Failed to open: ${String(err)}`);
    }
  }

  async function openWithFallback() {
    const target = primaryEntry();
    if (!target) {
      return;
    }

    try {
      await invoke('open_with', { path: target.path });
    } catch (err) {
      showToast(`Open With failed: ${String(err)}`);
    }
  }

  async function revealSelected() {
    const paths = selectedPaths();
    if (paths.length === 0) {
      return;
    }

    try {
      await invoke('reveal_in_finder', { paths });
    } catch (err) {
      showToast(`Failed to reveal in Finder: ${String(err)}`);
    }
  }

  async function copySelectedPaths() {
    const paths = selectedPaths();
    if (paths.length === 0) {
      return;
    }

    try {
      await invoke('copy_paths', { paths });
      showToast(`Copied ${paths.length} path(s)`);
    } catch (err) {
      showToast(`Failed to copy path: ${String(err)}`);
    }
  }

  async function copyFiles() {
    const paths = selectedPaths();
    if (paths.length === 0) {
      return;
    }

    try {
      await invoke('copy_files', { paths });
      showToast(`Copied ${paths.length} item(s)`);
    } catch (err) {
      showToast(`Failed to copy: ${String(err)}`);
    }
  }

  async function trashSelected() {
    const paths = selectedPaths();
    if (paths.length === 0) {
      return;
    }

    const message =
      paths.length === 1
        ? 'Move selected item to Trash?'
        : `Move ${paths.length} items to Trash?`;

    if (!window.confirm(message)) {
      return;
    }

    try {
      await invoke('move_to_trash', { paths });
      showToast('Moved to Trash.');
      clearSelection();
      await runSearch();
    } catch (err) {
      showToast(`Failed to move to Trash: ${String(err)}`);
    }
  }

  async function resetIndex() {
    if (indexStatus.state === 'Indexing') {
      showToast('Indexing in progress. Please try again after it completes.');
      return;
    }

    results = [];
    totalResults = 0;
    totalResultsKnown = false;
    clearSelection();
    scanned = 0;
    indexed = 0;
    currentPath = '';
    dbLatencyMs = null;
    dbLastQuery = '';
    indexStatus = { ...indexStatus, state: 'Indexing', entriesCount: 0, message: null };
    indexingStartTime = Date.now();
    indexingElapsed = '';
    indexingFinishedAt = '';

    resetInFlight = true;
    try {
      await invoke('reset_index');
    } catch (err) {
      showToast(`Failed to reset index: ${String(err)}`);
      await refreshStatus();
    } finally {
      resetInFlight = false;
    }
  }

  function isMultiSelected() {
    return selectedIndices.size > 1;
  }

  async function startRename() {
    if (isMultiSelected()) {
      return;
    }

    const idx = lastSelectedIndex;
    if (idx < 0 || !results[idx]) {
      return;
    }

    const entry = results[idx];
    editing = {
      active: true,
      path: entry.path,
      index: idx,
      draftName: entry.name
    };

    await tick();

    if (renameInputEl) {
      renameInputEl.focus();
      const extPos = !entry.isDir ? entry.name.lastIndexOf('.') : -1;
      const selectionEnd = extPos > 0 ? extPos : entry.name.length;
      renameInputEl.setSelectionRange(0, selectionEnd);
    }
  }

  function cancelRename() {
    editing = {
      active: false,
      path: '',
      index: -1,
      draftName: ''
    };
  }

  async function commitRename() {
    if (!editing.active || editing.index < 0 || !results[editing.index]) {
      return;
    }

    const current = results[editing.index];
    const nextName = editing.draftName;

    try {
      await invoke('rename', {
        path: current.path,
        newName: nextName
      });
      cancelRename();
      await runSearch();
    } catch (err) {
      showToast(`Failed to rename: ${String(err)}`);
      await tick();
      renameInputEl?.focus();
    }
  }

  function onGlobalClick() {
    closeContextMenu();
  }

  async function focusSearch() {
    searchInputEl?.focus();
    searchInputEl?.select();
  }

  function isTextInputTarget(target) {
    return (
      target instanceof HTMLInputElement ||
      target instanceof HTMLTextAreaElement ||
      target?.isContentEditable
    );
  }

  async function handleKeydown(event) {
    const isMetaSelectAll = (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'a';
    const isMetaCopy = (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'c';
    const target = event.target;
    const isTextInput = isTextInputTarget(target);

    if (isMetaCopy && isTextInput) {
      return;
    }

    if (isMetaSelectAll) {
      if (isTextInput) {
        event.preventDefault();
        if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
          target.select();
        }
        return;
      }
    }

    if (editing.active) {
      if (event.key === 'Enter') {
        event.preventDefault();
        await commitRename();
        return;
      }

      if (event.key === 'Escape') {
        event.preventDefault();
        cancelRename();
        return;
      }
      return;
    }

    if (event.key === 'Escape') {
      event.preventDefault();
      clearSelection();
      searchInputEl?.focus();
      return;
    }

    if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveSelection(1, event.shiftKey);
      return;
    }

    if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveSelection(-1, event.shiftKey);
      return;
    }

    if (event.key === 'PageDown') {
      event.preventDefault();
      moveSelection(Math.floor(viewportHeight / rowHeight), event.shiftKey);
      return;
    }

    if (event.key === 'PageUp') {
      event.preventDefault();
      moveSelection(-Math.floor(viewportHeight / rowHeight), event.shiftKey);
      return;
    }

    if (event.key === 'Enter') {
      event.preventDefault();
      if (platform === 'windows') {
        await openSelected();
      } else {
        await startRename();
      }
      return;
    }

    if (event.key === 'F2') {
      event.preventDefault();
      await startRename();
      return;
    }

    if (event.key === ' ' && !isTextInput) {
      event.preventDefault();
      const entry = primaryEntry();
      if (entry) {
        await invoke('quick_look', { path: entry.path });
      }
      return;
    }

    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'f') {
      event.preventDefault();
      clearSelection();
      searchInputEl?.focus();
      searchInputEl?.select();
      return;
    }

    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'o') {
      event.preventDefault();
      await openSelected();
      return;
    }

    if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
      event.preventDefault();
      await revealSelected();
      return;
    }

    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'c') {
      event.preventDefault();
      await copySelectedPaths();
      return;
    }

    if (isMetaSelectAll) {
      event.preventDefault();
      selectedIndices = new Set(Array.from({ length: results.length }, (_, i) => i));
      return;
    }

    if (event.key === 'Delete' || ((event.metaKey || event.ctrlKey) && event.key === 'Backspace')) {
      if (!isTextInput) {
        event.preventDefault();
        await trashSelected();
      }
      return;
    }
  }

  function formatSize(entry) {
    if (entry.isDir || entry.size == null) return '';
    const bytes = entry.size;
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  }

  function formatModified(entry) {
    if (!entry.mtime) {
      return '';
    }

    const d = new Date(entry.mtime * 1000);
    const pad = (n) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  function formatLastUpdated(timestamp) {
    if (!timestamp) {
      return '-';
    }

    return new Date(timestamp * 1000).toLocaleString();
  }

  function formatElapsed(ms) {
    const totalSec = Math.floor(ms / 1000);
    const min = Math.floor(totalSec / 60);
    const sec = totalSec % 60;
    return min > 0 ? `${min}m ${sec}s` : `${sec}s`;
  }

  function updateElapsed() {
    if (indexingStartTime && indexStatus.state === 'Indexing') {
      indexingElapsed = formatElapsed(Date.now() - indexingStartTime);
    }
  }

  function startElapsedTimer() {
    clearInterval(elapsedTimer);
    indexingStartTime = Date.now();
    indexingElapsed = '0s';
    indexingFinishedAt = '';
    elapsedTimer = setInterval(updateElapsed, 1000);
  }

  function stopElapsedTimer() {
    clearInterval(elapsedTimer);
    if (indexingStartTime) {
      const elapsed = Date.now() - indexingStartTime;
      indexingFinishedAt = `${formatElapsed(elapsed)}`;
    }
    indexingStartTime = null;
    indexingElapsed = '';
  }

  function displayPath(path) {
    if (!path) {
      return path;
    }

    if (!homePrefix) {
      return path;
    }

    if (path === homePrefix) {
      return '~';
    }

    if (path.startsWith(`${homePrefix}/`)) {
      return `~${path.slice(homePrefix.length)}`;
    }

    return path;
  }

  let unlistenFns = [];

  onMount(async () => {
    const t0 = performance.now();
    const ms = () => Math.round(performance.now() - t0);
    const step = async (name, fn) => {
      const startedAt = performance.now();
      startupLog(`[startup/fe] +${ms()}ms ${name} start`);
      try {
        const result = await fn();
        startupLog(`[startup/fe] +${ms()}ms ${name} done (${Math.round(performance.now() - startedAt)}ms)`);
        return result;
      } catch (err) {
        startupLog(
          `[startup/fe] +${ms()}ms ${name} failed (${Math.round(performance.now() - startedAt)}ms): ${String(err)}`
        );
        throw err;
      }
    };

    startupLog('[startup/fe] onMount entered');

    updateViewportHeight();
    startupLog(`[startup/fe] +${ms()}ms updateViewportHeight done`);

    platform = await step('invoke(get_platform)', () => invoke('get_platform'));

    const unlistenProgress = await step(
      'listen(index_progress)',
      () => listen('index_progress', (event) => {
        scanned = event.payload.scanned;
        indexed = event.payload.indexed;
        currentPath = event.payload.currentPath;
        if (indexStatus.state !== 'Indexing') {
          indexStatus = {
            ...indexStatus,
            state: 'Indexing'
          };
        }
      })
    );

    const unlistenState = await step(
      'listen(index_state)',
      () => listen('index_state', (event) => {
        startupLog(`[startup/fe] index_state event received: ${event.payload.state} at +${ms()}ms`);
        const prevState = indexStatus.state;
        indexStatus = {
          ...indexStatus,
          state: event.payload.state,
          message: event.payload.message ?? null,
          isCatchup: event.payload.isCatchup ?? false
        };

        if (event.payload.state === 'Indexing' && prevState !== 'Indexing') {
          if (prevState === 'Ready') {
            lastReadyCount = indexStatus.entriesCount;
          }
          startElapsedTimer();
        } else if (event.payload.state !== 'Indexing' && prevState === 'Indexing') {
          stopElapsedTimer();
        }

        if (event.payload.state === 'Ready') {
          scheduleSearch(true);
        }
      })
    );

    const unlistenUpdated = await step(
      'listen(index_updated)',
      () => listen('index_updated', (event) => {
        indexStatus = {
          ...indexStatus,
          entriesCount: event.payload.entriesCount,
          lastUpdated: event.payload.lastUpdated,
          permissionErrors: event.payload.permissionErrors ?? indexStatus.permissionErrors
        };

        scheduleSearch(true);
      })
    );

    const unlistenFocus = await step('listen(focus_search)', () => listen('focus_search', () => {
      void focusSearch();
    }));

    const unlistenCtxMenuAction = await step(
      'listen(context_menu_action)',
      () => listen('context_menu_action', (event) => {
        switch (event.payload) {
          case 'open': void openSelected(); break;
          case 'quick_look': {
            const e = primaryEntry();
            if (e) void invoke('quick_look', { path: e.path });
            break;
          }
          case 'open_with': void openWithFallback(); break;
          case 'reveal': void revealSelected(); break;
          case 'copy_files': void copyFiles(); break;
          case 'copy_path': void copySelectedPaths(); break;
          case 'trash': void trashSelected(); break;
          case 'rename': void startRename(); break;
        }
      })
    );

    unlistenFns = [unlistenProgress, unlistenState, unlistenUpdated, unlistenFocus, unlistenCtxMenuAction];
    startupLog(`[startup/fe] +${ms()}ms all listeners registered`);

    // Fetch backend state IMMEDIATELY after listeners are registered.
    // Events emitted before listener registration are lost, so this
    // is the only reliable way to get the current state.
    await step('refreshStatus()', () => refreshStatus());

    // Show Svelte UI: remove skeleton, focus search input.
    await step('tick()', () => tick());
    document.getElementById('skeleton')?.remove();
    await step('invoke(mark_frontend_ready)', () => invoke('mark_frontend_ready'));
    await step('focusSearch()', () => focusSearch());
    startupLog(`[startup/fe] +${ms()}ms WINDOW VISIBLE - skeleton removed`);

    try {
      homePrefix = await step('invoke(get_home_dir)', () => invoke('get_home_dir'));
    } catch {
      homePrefix = '';
    }
    startupLog(`[startup/fe] +${ms()}ms get_home_dir done`);

    if (!localStorage.getItem('everything-fda-notice-v1')) {
      showToast('macOS Full Disk Access permission may be required for full disk search.');
      localStorage.setItem('everything-fda-notice-v1', '1');
    }
    window.addEventListener('resize', updateViewportHeight);
    window.addEventListener('click', onGlobalClick);

    if (tableContainer) {
      tableContainer.setAttribute('data-overlayscrollbars-initialize', '');
      osInstance = OverlayScrollbars(tableContainer, {
        scrollbars: {
          theme: 'os-theme-dark',
          autoHide: 'scroll',
          autoHideDelay: 500,
          clickScroll: true
        }
      });
      osViewport = osInstance.elements().viewport;

      let lastScrollY = 0;
      let lastScrollX = 0;
      let scrollTimeoutY = null;
      let scrollTimeoutX = null;

      const onScroll = () => {
        scrollTop = osViewport.scrollTop;
        headerScrollLeft = osViewport.scrollLeft;

        if (scrollTop !== lastScrollY) {
          tableContainer.classList.add('scrolling-y');
          tableContainer.classList.remove('scrolling-x');
          clearTimeout(scrollTimeoutY);
          scrollTimeoutY = setTimeout(() => {
            if (tableContainer) tableContainer.classList.remove('scrolling-y');
          }, 1500);
        }
        if (headerScrollLeft !== lastScrollX) {
          tableContainer.classList.add('scrolling-x');
          tableContainer.classList.remove('scrolling-y');
          clearTimeout(scrollTimeoutX);
          scrollTimeoutX = setTimeout(() => {
            if (tableContainer) tableContainer.classList.remove('scrolling-x');
          }, 1500);
        }
        lastScrollY = scrollTop;
        lastScrollX = headerScrollLeft;

        const scrollBottom = scrollTop + osViewport.clientHeight;
        if (scrollBottom >= totalHeight - rowHeight * 10) {
          void loadMore();
        }
      };

      osViewport.addEventListener('scroll', onScroll);
      scrollCleanup = () => {
        clearTimeout(scrollTimeoutY);
        clearTimeout(scrollTimeoutX);
        osViewport.removeEventListener('scroll', onScroll);
      };
    }

    await step('runSearch()', () => runSearch());

    statusRefreshTimer = setInterval(() => {
      if (indexStatus.state === 'Indexing') {
        void refreshStatus();
      }
    }, 1000);
    startupLog(`[startup/fe] +${ms()}ms onMount complete`);
    console.log(`[startup/fe] +${ms()}ms onMount complete`);
  });

  onDestroy(async () => {
    clearTimeout(searchTimer);
    clearTimeout(toastTimer);
    clearInterval(statusRefreshTimer);
    clearInterval(elapsedTimer);
    resizeCleanup?.();

    for (const unlisten of unlistenFns) {
      unlisten();
    }

    window.removeEventListener('resize', updateViewportHeight);
    window.removeEventListener('click', onGlobalClick);
    scrollCleanup?.();
    if (osInstance) {
      osInstance.destroy();
    }
  });
</script>

<svelte:window on:keydown={handleKeydown} />

<div class="app-shell">
  <header class="search-bar">
    <input
      bind:this={searchInputEl}
      class="search-input"
      type="text"
      bind:value={query}
      on:input={scheduleSearch}
      on:focus={clearSelection}
      placeholder="Search file/folder names"
      autocomplete="off"
      spellcheck="false"
    />
  </header>

  <section class="table-area" bind:this={tableAreaEl} style={tableGridStyle}>
    <div class="table-header">
      <div class="table-header-track">
        <div class="col name">
          <button type="button" class="col-button" on:click={() => handleHeaderSort('name')}>
            Name{#if sortBy === 'name'}{sortDir === 'asc' ? ' ▲' : ' ▼'}{/if}
          </button>
          <button
            type="button"
            class="col-resizer"
            class:active={resizingColumn === 'name'}
            on:mousedown={(event) => startColumnResize(event, 'name')}
            aria-label="Resize Name column"
          />
        </div>

        <div class="col path">
          <button type="button" class="col-button" on:click={() => handleHeaderSort('dir')}>
            Path{#if sortBy === 'dir'}{sortDir === 'asc' ? ' ▲' : ' ▼'}{/if}
          </button>
          <button
            type="button"
            class="col-resizer"
            class:active={resizingColumn === 'path'}
            on:mousedown={(event) => startColumnResize(event, 'path')}
            aria-label="Resize Path column"
          />
        </div>

        <div class="col size">
          <button type="button" class="col-button" on:click={() => handleHeaderSort('size')}>
            Size{#if sortBy === 'size'}{sortDir === 'asc' ? ' ▲' : ' ▼'}{/if}
          </button>
        </div>

        <div class="col modified">
          <button type="button" class="col-button" on:click={() => handleHeaderSort('mtime')}>
            Modified{#if sortBy === 'mtime'}{sortDir === 'asc' ? ' ▲' : ' ▼'}{/if}
          </button>
        </div>
      </div>
    </div>

    <div
      class="table-body"
      bind:this={tableContainer}
    >
      <div class="spacer" style={`height:${totalHeight}px`}>
        <div class="rows" style={`transform: translateY(${translateY}px);`}>
          {#each visibleRows as entry, localIndex}
            {@const index = startIndex + localIndex}
            <div
              class="row {selectedIndices.has(index) ? 'selected' : ''}"
              on:mousedown={(event) => handleRowMouseDown(event, index)}
              on:click={(event) => handleRowClick(event, index)}
              on:keydown={(event) => handleRowA11yKeydown(event, index)}
              on:dblclick={() => handleRowDoubleClick(index)}
              on:contextmenu={(event) => handleRowContextMenu(event, index)}
              role="row"
              tabindex="0"
            >
              <div class="cell name">
                <img class="file-icon" src={iconFor(entry)} alt="" />

                {#if editing.active && editing.index === index}
                  <input
                    bind:this={renameInputEl}
                    class="rename-input"
                    bind:value={editing.draftName}
                    on:click|stopPropagation
                  />
                {:else}
                  <span class="ellipsis">{#each highlightSegments(entry.name, query) as seg}{#if seg.hl}<mark class="hl">{seg.text}</mark>{:else}{seg.text}{/if}{/each}</span>
                {/if}
              </div>
              <div class="cell path"><span class="ellipsis">{displayPath(entry.dir)}</span></div>
              <div class="cell size">{formatSize(entry)}</div>
              <div class="cell modified">{formatModified(entry)}</div>
            </div>
          {/each}
        </div>
      </div>
    </div>
  </section>

  <footer class="status-bar">
    {#if indexStatus.state === 'Indexing'}
        {#if indexStatus.entriesCount > 0}
          <span>Indexing{#if lastReadyCount > 0} ({Math.min(99, Math.round((scanned / lastReadyCount) * 100))}%){/if}{#if indexingElapsed} · {indexingElapsed}{/if} · {indexStatus.entriesCount.toLocaleString()} entries</span>
        {:else}
          <span>Starting indexing...{#if indexingElapsed} ({indexingElapsed}){/if}</span>
        {/if}
      {:else}
        <span>Index: {indexStatus.state}</span>
        <span>Entries: {indexStatus.entriesCount.toLocaleString()}</span>
        {#if indexingFinishedAt}
          <span>Indexed in {indexingFinishedAt}</span>
        {/if}
      {/if}
      {#if searchModeLabel === 'spotlight' || searchModeLabel === 'spotlight_timeout'}
        <span class="status-spotlight">Spotlight fallback{#if searchModeLabel === 'spotlight_timeout'} (partial results){/if}</span>
      {/if}
      {#if dbLatencyMs !== null && dbLastQuery}
        <span>"{dbLastQuery}" {dbLatencyMs} ms · {totalResults} results</span>
      {/if}
    <button
      class="status-btn"
      on:click={resetIndex}
      disabled={indexStatus.state === 'Indexing'}
      title={indexStatus.state === 'Indexing' ? 'Cannot reset while indexing is in progress.' : 'Reset and rebuild the index.'}
    >
      Reset Index
    </button>
    {#if indexStatus.state === 'Indexing' && !indexStatus.isCatchup}
      <span class="index-progress">Scanned {scanned.toLocaleString()} / Indexed {indexed.toLocaleString()}</span>
      <span class="path-preview">{displayPath(currentPath)}</span>
    {/if}
    {#if indexStatus.permissionErrors > 0}
      <span class="status-warning">Permission errors: {indexStatus.permissionErrors.toLocaleString()}</span>
    {/if}
    {#if indexStatus.message}
      <span class={indexStatus.isCatchup ? 'index-progress' : 'status-error'}>{indexStatus.message}</span>
    {/if}
  </footer>

  {#if contextMenu.visible}
    <div class="context-menu" style={`left:${contextMenu.x}px;top:${contextMenu.y}px;`}>
      <button on:click={() => (closeContextMenu(), openSelected())}>Open</button>
      <button on:click={async () => { closeContextMenu(); const e = primaryEntry(); if (e) await invoke('quick_look', { path: e.path }); }}>Quick Look</button>
      <button on:click={() => (closeContextMenu(), openWithFallback())}>Open With... (Reveal in Finder)</button>
      <button on:click={() => (closeContextMenu(), revealSelected())}>Reveal in Finder</button>
      <button on:click={() => (closeContextMenu(), copySelectedPaths())}>Copy Path</button>
      <button on:click={() => (closeContextMenu(), trashSelected())}>Move to Trash</button>
      {#if selectedIndices.size === 1}
        <button on:click={() => (closeContextMenu(), startRename())}>Rename</button>
      {/if}
    </div>
  {/if}

  {#if toast}
    <div class="toast">{toast}</div>
  {/if}
</div>

<style>
  :global(:root) {
    color-scheme: light dark;
    --bg-app: transparent;
    --text-primary: #0d1826;
    --text-muted: #48596c;
    --bar-grad-top: rgba(255, 255, 255, 0.48);
    --bar-grad-bottom: rgba(248, 250, 253, 0.42);
    --surface-header: rgba(252, 252, 255, 0.40);
    --surface: rgba(255, 255, 255, 0.22);
    --border-soft: rgba(0, 0, 0, 0.07);
    --border-input: rgba(0, 0, 0, 0.13);
    --focus-ring: #6f96e6;
    --row-border: rgba(0, 0, 0, 0.032);
    --row-hover: rgba(0, 0, 0, 0.06);
    --row-selected: rgba(0, 0, 0, 0.12);
    --button-bg: rgba(255, 255, 255, 0.62);
    --button-border: rgba(0, 0, 0, 0.10);
    --button-text: #2c3e50;
    --menu-bg: rgba(240, 241, 248, 0.90);
    --menu-border: rgba(0, 0, 0, 0.06);
    --menu-hover: rgba(255, 255, 255, 0.62);
    --menu-text: #1a2a38;
    --error-text: #b64545;
    --warning-text: #8f6500;
    --toast-bg: rgba(18, 22, 32, 0.90);
    --toast-text: #ffffff;
  }

  @media (prefers-color-scheme: dark) {
    :global(:root) {
      --bg-app: transparent;
      --text-primary: #e6e6ea;
      --text-muted: #9494a4;
      --bar-grad-top: rgba(38, 38, 46, 0.52);
      --bar-grad-bottom: rgba(30, 30, 38, 0.46);
      --surface-header: rgba(32, 32, 40, 0.46);
      --surface: rgba(18, 18, 26, 0.32);
      --border-soft: rgba(255, 255, 255, 0.065);
      --border-input: rgba(255, 255, 255, 0.14);
      --focus-ring: #5b8bd9;
      --row-border: rgba(255, 255, 255, 0.026);
      --row-hover: rgba(255, 255, 255, 0.08);
      --row-selected: rgba(255, 255, 255, 0.14);
      --button-bg: rgba(255, 255, 255, 0.072);
      --button-border: rgba(255, 255, 255, 0.11);
      --button-text: #d6d6e0;
      --menu-bg: rgba(36, 36, 46, 0.90);
      --menu-border: rgba(255, 255, 255, 0.066);
      --menu-hover: rgba(255, 255, 255, 0.062);
      --menu-text: #e0e0e8;
      --error-text: #ff9d9d;
      --warning-text: #e0c670;
      --toast-bg: rgba(10, 10, 16, 0.93);
      --toast-text: #f2f2f6;
    }
  }

  :global(html, body) {
    margin: 0;
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: transparent;
    color: var(--text-primary);
    font-family: 'SF Pro Text', 'Segoe UI', Helvetica, Arial, sans-serif;
  }

  :global(#app) {
    width: 100%;
    height: 100%;
  }

  .app-shell {
    display: grid;
    grid-template-rows: auto 1fr auto;
    height: 100%;
    min-width: 0;
  }

  .search-bar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 8px;
    background: linear-gradient(180deg, var(--bar-grad-top) 0%, var(--bar-grad-bottom) 100%);
    border-bottom: 1px solid var(--border-soft);
    min-width: 0;
  }

  .search-input {
    display: block;
    box-sizing: border-box;
    width: 100%;
    min-width: 0;
    flex: 1 1 auto;
    height: 32px;
    border: 1px solid var(--border-input);
    border-radius: 8px;
    padding: 0 10px;
    font-size: 14px;
    background: var(--surface);
    color: var(--text-primary);
    backdrop-filter: blur(12px) saturate(160%);
    -webkit-backdrop-filter: blur(12px) saturate(160%);
  }

  .search-input::placeholder {
    color: var(--text-muted);
    opacity: 0.85;
    font-size: 12px;
  }

  .search-input:focus {
    outline: none;
    border-color: var(--focus-ring);
    box-shadow: 0 0 0 2px rgba(125, 169, 255, 0.25);
  }

  .table-area {
    display: grid;
    grid-template-rows: auto 1fr;
    min-height: 0;
    min-width: 0;
  }

  .table-header-track,
  .row {
    display: grid;
    grid-template-columns: var(--col-name) var(--col-path) var(--col-size) var(--col-modified);
    align-items: center;
    width: max(var(--table-min-width, 0px), 100%);
  }

  .table-header {
    height: 30px;
    border-bottom: 1px solid var(--border-soft);
    background: var(--surface-header);
    user-select: none;
    overflow: hidden;
  }

  .table-header-track {
    height: 100%;
    transform: translateX(var(--header-offset, 0px));
    will-change: transform;
  }

  .table-header .col {
    position: relative;
    min-width: 0;
    height: 100%;
  }

  .col-button {
    width: 100%;
    height: 100%;
    display: flex;
    align-items: center;
    justify-content: flex-start;
    box-sizing: border-box;
    padding: 0 8px;
    font-size: 12px;
    font-weight: 600;
    line-height: 1;
    color: var(--text-muted);
    text-align: left;
    margin: 0;
    border: none;
    background: transparent;
    cursor: pointer;
  }

  .col-resizer {
    position: absolute;
    top: 0;
    right: -4px;
    width: 8px;
    height: 100%;
    border: none;
    padding: 0;
    background: transparent;
    cursor: col-resize;
    z-index: 30;
  }

  .col-resizer::after {
    content: '';
    position: absolute;
    top: 7px;
    bottom: 7px;
    left: 50%;
    width: 1px;
    transform: translateX(-50%);
    background: var(--border-soft);
    opacity: 0;
    transition: opacity 0.15s ease;
  }

  .table-header .col:hover .col-resizer::after,
  .col-resizer.active::after {
    opacity: 1;
  }

  .table-body {
    overflow: auto;
    overflow-anchor: none;
    min-height: 0;
    min-width: 0;
    background: var(--surface);
  }

  .spacer {
    position: relative;
    width: max(var(--table-min-width, 0px), 100%);
  }

  .rows {
    position: absolute;
    top: 0;
    left: 0;
    width: max(var(--table-min-width, 0px), 100%);
  }

  .row {
    height: 28px;
    border-bottom: 1px solid var(--row-border);
    cursor: default;
    -webkit-user-select: none;
    user-select: none;
    outline: none;
  }

  .row:hover {
    background: var(--row-hover);
  }

  .row.selected {
    background: var(--row-selected);
  }

  .cell {
    padding: 0 8px;
    font-size: 12px;
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .cell.name {
    min-width: 0;
  }

  .cell.path,
  .cell.size,
  .cell.modified {
    min-width: 0;
  }

  .cell.size {
    text-align: right;
    padding-right: 8px;
    font-variant-numeric: tabular-nums;
  }

  .ellipsis {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .hl {
    background: none;
    color: inherit;
    font-weight: 700;
  }

  .file-icon {
    width: 16px;
    height: 16px;
    flex: 0 0 auto;
  }

  .rename-input {
    width: 100%;
    height: 20px;
    font-size: 12px;
    border: 1px solid var(--focus-ring);
    border-radius: 4px;
    padding: 0 4px;
    background: var(--surface);
    color: var(--text-primary);
  }

  .status-bar {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 6px 8px;
    background: var(--surface-header);
    border-top: 1px solid var(--border-soft);
    font-size: 11px;
    color: var(--text-muted);
    white-space: nowrap;
    overflow: hidden;
    min-width: 0;
  }

  .status-btn {
    border: 1px solid var(--button-border);
    border-radius: 5px;
    background: var(--button-bg);
    color: var(--button-text);
    font-size: 11px;
    height: 22px;
    padding: 0 8px;
    cursor: pointer;
  }

  .status-btn:hover {
    filter: brightness(1.06);
  }

  .status-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
    filter: none;
  }

  .path-preview {
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .status-error {
    color: var(--error-text);
  }

  .status-warning {
    color: var(--warning-text);
  }

  .status-spotlight {
    color: #f59e0b;
    font-weight: 600;
  }

  .context-menu {
    position: fixed;
    z-index: 200;
    width: 220px;
    border: 1px solid var(--menu-border);
    border-radius: 10px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.16), inset 0 0 0 0.5px rgba(255, 255, 255, 0.18);
    background: var(--menu-bg);
    backdrop-filter: blur(28px) saturate(180%);
    -webkit-backdrop-filter: blur(28px) saturate(180%);
    overflow: hidden;
  }

  .context-menu button {
    width: 100%;
    text-align: left;
    border: none;
    background: transparent;
    padding: 8px 10px;
    font-size: 12px;
    color: var(--menu-text);
  }

  .context-menu button:hover {
    background: var(--menu-hover);
  }

  .toast {
    position: fixed;
    right: 14px;
    bottom: 18px;
    background: var(--toast-bg);
    color: var(--toast-text);
    border-radius: 10px;
    padding: 9px 12px;
    font-size: 12px;
    z-index: 250;
    backdrop-filter: blur(20px) saturate(180%);
    -webkit-backdrop-filter: blur(20px) saturate(180%);
    box-shadow: 0 4px 20px rgba(0, 0, 0, 0.2);
  }

  /* macOS Style OverlayScrollbars */
  :global(.os-theme-dark) {
    --os-size: 10px; /* Default width */
    --os-handle-border-radius: 10px;
    --os-track-border-radius: 10px;
    
    /* Handle colors */
    --os-handle-bg: rgba(0, 0, 0, 0.25);
    --os-handle-bg-hover: rgba(0, 0, 0, 0.35);
    --os-handle-bg-active: rgba(0, 0, 0, 0.45);
    
    --os-handle-border: rgba(0, 0, 0, 0.25);
    --os-handle-border-hover: rgba(0, 0, 0, 0.35);
    --os-handle-border-active: rgba(0, 0, 0, 0.45);

    /* Track background is transparent by default */
    --os-track-bg: transparent;
    --os-track-bg-hover: rgba(0, 0, 0, 0.02);
    --os-track-bg-active: rgba(0, 0, 0, 0.04);

    --os-track-border: transparent;
    --os-track-border-hover: rgba(0, 0, 0, 0.02);
    --os-track-border-active: rgba(0, 0, 0, 0.04);
  }

  @media (prefers-color-scheme: dark) {
    :global(.os-theme-dark) {
      --os-handle-bg: rgba(255, 255, 255, 0.25);
      --os-handle-bg-hover: rgba(255, 255, 255, 0.35);
      --os-handle-bg-active: rgba(255, 255, 255, 0.45);
      
      --os-handle-border: rgba(255, 255, 255, 0.25);
      --os-handle-border-hover: rgba(255, 255, 255, 0.35);
      --os-handle-border-active: rgba(255, 255, 255, 0.45);
      
      --os-track-bg-hover: rgba(255, 255, 255, 0.02);
      --os-track-bg-active: rgba(255, 255, 255, 0.04);

      --os-track-border-hover: rgba(255, 255, 255, 0.02);
      --os-track-border-active: rgba(255, 255, 255, 0.04);
    }
  }

  /* Ensure scrollbar hits the very edges of the container, ignoring intersection */
  :global(.os-scrollbar.os-scrollbar-vertical) {
    bottom: 0 !important;
  }
  :global(.os-scrollbar.os-scrollbar-horizontal) {
    right: 0 !important;
  }

  /* Smooth transitions for size and track color, instant fade-in */
  :global(.os-scrollbar) {
    transition: width 0.3s ease, height 0.3s ease, opacity 0s, visibility 0s, background-color 0.3s ease !important;
  }
  
  /* Gradual fade-out only when hiding */
  :global(.os-scrollbar.os-scrollbar-auto-hide-hidden) {
    transition: width 0.3s ease, height 0.3s ease, opacity 0.4s ease-in-out, visibility 0.4s ease-in-out, background-color 0.3s ease !important;
  }

  /* Make sure only the active axis stays visible during scrolling or mouse interaction */
  :global(.scrolling-y .os-scrollbar-horizontal),
  :global(.scrolling-x .os-scrollbar-vertical),
  :global([data-overlayscrollbars-initialize]:has(.os-scrollbar-vertical:hover) .os-scrollbar-horizontal),
  :global([data-overlayscrollbars-initialize]:has(.os-scrollbar-vertical.os-scrollbar-interacting) .os-scrollbar-horizontal),
  :global([data-overlayscrollbars-initialize]:has(.os-scrollbar-horizontal:hover) .os-scrollbar-vertical),
  :global([data-overlayscrollbars-initialize]:has(.os-scrollbar-horizontal.os-scrollbar-interacting) .os-scrollbar-vertical) {
    opacity: 0 !important;
    visibility: hidden !important;
    transition: opacity 0.4s ease-in-out, visibility 0.4s ease-in-out !important;
  }

  :global(.os-scrollbar-track),
  :global(.os-scrollbar-handle) {
    transition: width 0.3s ease, height 0.3s ease, background-color 0.3s ease !important;
  }

  /* Expand size and show track background when hovering over the scrollbar */
  :global(.os-scrollbar:hover),
  :global(.os-scrollbar:active),
  :global(.os-scrollbar.os-scrollbar-interacting) {
    --os-size: 15px; /* Expanded width */
    background-color: var(--os-track-bg-hover);
    border-radius: var(--os-track-border-radius);
  }

  :global(.os-scrollbar.os-scrollbar-interacting) {
    background-color: var(--os-track-bg-active);
  }
</style>
