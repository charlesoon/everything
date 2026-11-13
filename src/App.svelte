<script>
  import { onDestroy, onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { startDrag } from '@crabnebula/tauri-plugin-drag';
  import 'overlayscrollbars/overlayscrollbars.css';
  import { OverlayScrollbars } from 'overlayscrollbars';
  import { excludeTrashedEntries } from './file-actions';
  import {
    SEARCH_DEBOUNCE_MS,
    computeSearchFetchLimit,
    extractLeafQuery,
    shouldApplyPreserveResults
  } from './search-utils';
  let osInstance = null;
  let osViewport = null;
  let scrollCleanup = null;
  function getScrollEl() {
    return osViewport || tableContainer;
  }

  const rowHeight = 26;
  const PAGE_SIZE = 500;
  let homePrefix = '';
  const COL_WIDTHS_KEY = 'everything-col-widths-v3';
  const columnKeys = ['name', 'path', 'size', 'modified'];
  const minColumnWidth = {
    name: 180,
    path: 240,
    size: 62,
    modified: 150
  };
  const defaultColumnRatios = {
    name: 0.32,
    path: 0.48,
    size: 0.05
  };
  const DEBUG_STARTUP = false;

  // Theme follows the system appearance; the backend call only syncs the
  // native window background color (avoids flash on resize).
  const themeMedia = window.matchMedia('(prefers-color-scheme: dark)');
  let theme = themeMedia.matches ? 'dark' : 'light';
  themeMedia.addEventListener('change', (e) => {
    theme = e.matches ? 'dark' : 'light';
    invoke('set_native_theme', { theme });
  });

  function sortIconHtml(dir) {
    const d = dir === 'asc' ? 'M2.5 6.5 L5 4 L7.5 6.5' : 'M2.5 3.5 L5 6 L7.5 3.5';
    return `<svg class="sort-icon" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="${d}"/></svg>`;
  }

  // The backend can force a webview reload to recover a window whose layer tree
  // WebKit stranded hidden (src-tauri/src/ui_gate.rs). That remounts the page,
  // so the query and sort ride sessionStorage — which a reload keeps and a fresh
  // app launch starts empty — and the recovery stays invisible to the user.
  // Read here, before any reactive statement can overwrite it.
  const SESSION_UI_KEY = 'everything.ui.session';
  const restoredUi = (() => {
    try {
      return JSON.parse(sessionStorage.getItem(SESSION_UI_KEY) ?? 'null');
    } catch (_) {
      return null;
    }
  })();

  let query = typeof restoredUi?.query === 'string' ? restoredUi.query : '';
  let results = [];
  let totalResults = 0;
  let totalResultsKnown = false;
  let totalResultsQuery = null;

  let dbLastQuery = '';
  let searchModeLabel = '';
  let selectedIndices = new Set();
  let selectionAnchor = -1;
  let lastSelectedIndex = -1;
  let pendingTrashedPaths = new Set();
  let loadedResultCount = 0;
  let trashInFlight = false;
  let trashDialogEl;
  let trashItemCount = 0;
  let resolveTrashDialog = null;
  let editing = {
    active: false,
    path: '',
    index: -1,
    draftName: ''
  };

  let renameOverlay = { top: 0, left: 0, maxWidth: 0 };

  let indexStatus = {
    state: 'Indexing',
    entriesCount: 0,
    lastUpdated: null,
    permissionErrors: 0,
    message: null,
    isCatchup: false,
    backgroundActive: false
  };

  let indexingStartTime = null;
  let indexingElapsed = '';
  let indexingFinishedAt = '';

  let sortBy = restoredUi?.sortBy ?? 'name';
  let sortDir = restoredUi?.sortDir ?? 'asc';

  $: {
    try {
      sessionStorage.setItem(SESSION_UI_KEY, JSON.stringify({ query, sortBy, sortDir }));
    } catch (_) {
      // Private-mode / quota failures only cost us the restore, not the search.
    }
  }

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
  let renameCommitting = false;
  let tableAreaEl;
  let tableContainer;
  let tableResizeObserver = null;
  let viewportResizeRaf = null;
  let scrollTop = 0;
  let headerScrollLeft = 0;
  let viewportHeight = 520;
  let lastTableWidth = 0;
  let resizingColumn = '';
  let resizeCleanup = null;
  let colWidths = {
    name: 250,
    path: 350,
    size: 62,
    modified: 150
  };

  let contextMenu = {
    visible: false,
    x: 0,
    y: 0
  };

  let platform = '';
  let isMaximized = false;
  let showFdaBanner = false;
  let showPathignoreBanner = false;
  let showPathindexingBanner = false;
  let serviceHealth = { ok: true, state: '' };
  let fdaSettingsOpened = false;
  const FDA_DISMISSED_KEY = 'everything-fda-dismissed';
  // macOS daemon-writer onboarding/auto-start state (get_onboarding_state DTO).
  let onboarding = null;
  let showOnboarding = false;
  let onboardingBusy = false;
  let fdaPollTimer = null;
  let autostartBusy = false;
  const appWindow = getCurrentWindow();
  let toast = '';
  let searchTimer;
  let lastSearchFiredAt = 0;
  let toastTimer;
  let statusRefreshTimer;
  let elapsedTimer;
  let bgPollTimer;
  let statusRefreshInFlight = false;
  let resetInFlight = false;
  let lastReadyCount = 0;
  const ICON_CACHE_MAX = 500;
  const ICON_RETRY_MAX_ATTEMPTS = 3;
  const ICON_RETRY_BASE_MS = 1500;
  const iconCache = new Map();
  const iconLoading = new Set();
  const iconRetry = new Map();
  const iconQueue = new Map();
  let iconVersion = 0;
  let iconRetryTick = 0;
  let iconFlushScheduled = false;

  const HIGHLIGHT_CACHE_MAX = 300;
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

  $: selectionInfo = buildSelectionInfo(selectedIndices, results);

  $: tableMinWidth = colWidths.name + colWidths.path + colWidths.size + colWidths.modified;
  $: tableGridStyle = `--col-name:${colWidths.name}px;--col-path:${colWidths.path}px;--col-size:${colWidths.size}px;--col-modified:${colWidths.modified}px;--table-min-width:${tableMinWidth}px;--header-offset:${-headerScrollLeft}px;`;

  $: {
    iconRetryTick;
    for (const entry of visibleRows) {
      queueIcon(entry);
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

    // Check cache (LRU: re-insert on access to keep recently used at end)
    const cached = highlightCache.get(name);
    if (cached) {
      highlightCache.delete(name);
      highlightCache.set(name, cached);
      return cached;
    }

    q = extractLeafQuery(q);
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
    if (highlightCache.size > HIGHLIGHT_CACHE_MAX) {
      const oldest = highlightCache.keys().next().value;
      highlightCache.delete(oldest);
    }
    return segs;
  }

  const perFileIconExts = new Set(['exe', 'lnk', 'ico', 'url', 'scr', 'appx']);

  function isAppBundle(entry) {
    return platform === 'macos' && entry.isDir && entry.name.toLowerCase().endsWith('.app');
  }

  function iconKey(entry) {
    if (isAppBundle(entry)) return entry.path;
    if (entry.isDir) return '__folder__';
    const ext = (entry.ext || '').toLowerCase();
    if (platform === 'windows' && perFileIconExts.has(ext)) return entry.path;
    return ext || '__file__';
  }

  // `_version` is intentionally unused: the table passes `iconVersion` here so
  // Svelte re-runs this binding when the icon cache updates (setIconCache bumps
  // iconVersion). Removing the param would make that call site look like a
  // mistake and invite silently breaking icon refresh.
  function iconFor(entry, _version = 0) {
    const key = iconKey(entry);
    const cached = iconCache.get(key);
    if (cached) {
      iconCache.delete(key);
      iconCache.set(key, cached);
      return cached;
    }
    return fallbackIconFor(entry);
  }

  function fallbackIconFor(entry) {
    return entry.isDir && !isAppBundle(entry) ? folderFallbackIcon : fileFallbackIcon;
  }

  function setIconCache(key, value) {
    iconCache.set(key, value);
    if (iconCache.size > ICON_CACHE_MAX) {
      const oldest = iconCache.keys().next().value;
      iconCache.delete(oldest);
    }
    iconVersion += 1;
  }

  // Icons are fetched in batches. On macOS every distinct icon the backend has
  // to render costs its own `swift -e` — a full compiler run, ~1s — so asking
  // row by row meant a screen of unseen extensions spawned one process each,
  // right while the first index was still running. One viewport pass now
  // collects its misses here and the backend renders the whole group together.
  function queueIcon(entry) {
    const key = iconKey(entry);
    if (iconCache.has(key) || iconLoading.has(key) || iconQueue.has(key)) {
      return;
    }
    const retry = iconRetry.get(key);
    if (retry && Date.now() < retry.at) {
      return;
    }
    iconQueue.set(key, entry);
    scheduleIconFlush();
  }

  function scheduleIconFlush() {
    // One batch in flight at a time — `iconLoading` holds exactly that batch's
    // keys, so a non-empty set means one is running. This is what keeps the
    // batches big: a scroll during a render coalesces into the next request
    // instead of sending its own one-key batch.
    if (iconFlushScheduled || iconLoading.size > 0 || iconQueue.size === 0) {
      return;
    }
    iconFlushScheduled = true;
    // A microtask rather than requestAnimationFrame: the visible-rows scan
    // enqueues the whole viewport synchronously, so this already batches it,
    // and rAF would stall icons whenever the window is occluded.
    queueMicrotask(flushIconQueue);
  }

  async function flushIconQueue() {
    iconFlushScheduled = false;
    if (iconQueue.size === 0) {
      return;
    }
    const batch = [...iconQueue];
    iconQueue.clear();
    for (const [key] of batch) {
      iconLoading.add(key);
    }
    try {
      // Positional: icons[i] belongs to batch[i], empty when nothing rendered.
      const icons = await invoke('get_file_icons', {
        requests: batch.map(([, entry]) => ({
          path: entry.path,
          ext: isAppBundle(entry) ? 'app' : entry.isDir ? 'folder' : entry.ext || '',
        })),
      });
      batch.forEach(([key, entry], i) => {
        const bytes = Array.isArray(icons) ? icons[i] : null;
        if (bytes && bytes.length > 0) {
          const image = `data:image/png;base64,${bytesToBase64(Uint8Array.from(bytes))}`;
          iconRetry.delete(key);
          setIconCache(key, image);
        } else {
          scheduleIconRetry(key, entry);
        }
      });
    } catch {
      for (const [key, entry] of batch) {
        scheduleIconRetry(key, entry);
      }
    } finally {
      for (const [key] of batch) {
        iconLoading.delete(key);
      }
      scheduleIconFlush();
    }
  }

  // Failed loads keep showing the fallback and retry with backoff; only after the
  // last attempt is the fallback cached for good.
  function scheduleIconRetry(key, entry) {
    const attempts = (iconRetry.get(key)?.attempts ?? 0) + 1;
    if (attempts >= ICON_RETRY_MAX_ATTEMPTS) {
      iconRetry.delete(key);
      setIconCache(key, fallbackIconFor(entry));
      return;
    }
    const delay = ICON_RETRY_BASE_MS * attempts;
    iconRetry.set(key, { at: Date.now() + delay, attempts });
    // Wake the visible-rows scan rather than retrying this entry directly, so
    // rows that scrolled away or belong to a stale search don't spend a load.
    setTimeout(() => {
      iconRetryTick += 1;
    }, delay);
  }

  function syncColumnWidthsToContainer() {
    const width = tableContainer ? getScrollEl().clientWidth : tableAreaEl?.clientWidth;
    if (!width) {
      return;
    }
    if (Math.abs(width - lastTableWidth) <= 1) {
      return;
    }

    if (lastTableWidth > 0) {
      const fixedModified = colWidths.modified;
      const currentFlexible = colWidths.name + colWidths.path + colWidths.size;
      const nextFlexible = Math.max(
        minColumnWidth.name + minColumnWidth.path + minColumnWidth.size,
        width - fixedModified
      );
      const scale = nextFlexible / Math.max(1, currentFlexible);
      let scaledName = Math.max(minColumnWidth.name, Math.round(colWidths.name * scale));
      let scaledSize = Math.max(minColumnWidth.size, Math.round(colWidths.size * scale));
      let scaledPath = nextFlexible - scaledName - scaledSize;
      if (scaledPath < minColumnWidth.path) {
        const shortfall = minColumnWidth.path - scaledPath;
        const nameReduction = Math.min(shortfall, scaledName - minColumnWidth.name);
        scaledName -= nameReduction;
        const remaining = shortfall - nameReduction;
        if (remaining > 0) {
          scaledSize -= Math.min(remaining, scaledSize - minColumnWidth.size);
        }
        scaledPath = nextFlexible - scaledName - scaledSize;
      }
      colWidths = {
        ...colWidths,
        name: scaledName,
        path: Math.max(minColumnWidth.path, scaledPath),
        size: scaledSize
      };
      lastTableWidth = width;
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

  function scheduleViewportHeightUpdate() {
    if (viewportResizeRaf !== null) {
      return;
    }
    viewportResizeRaf = requestAnimationFrame(() => {
      viewportResizeRaf = null;
      updateViewportHeight();
    });
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

  function replaceResults(entries) {
    const keepPaths = new Set(selectedPaths());
    const anchorPath = results[selectionAnchor]?.path;
    const primaryPath = results[lastSelectedIndex]?.path;
    results = entries;
    selectedIndices = new Set();
    selectionAnchor = -1;
    lastSelectedIndex = -1;
    for (let i = 0; i < results.length; i += 1) {
      if (keepPaths.has(results[i].path)) selectedIndices.add(i);
      if (results[i].path === anchorPath) selectionAnchor = i;
      if (results[i].path === primaryPath) lastSelectedIndex = i;
    }
    if (!selectedIndices.has(lastSelectedIndex)) lastSelectedIndex = [...selectedIndices].at(-1) ?? -1;
    if (!selectedIndices.has(selectionAnchor)) selectionAnchor = lastSelectedIndex;
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

  function invokeOnPrimary(cmd) {
    const e = primaryEntry();
    if (e) void invoke(cmd, { path: e.path });
  }

  function buildSelectionInfo(indices, rows) {
    if (indices.size === 1) {
      const entry = rows[[...indices][0]];
      if (!entry) return null;
      return displayPath(entry.path);
    }
    if (indices.size > 1) {
      let bytes = 0;
      let hasFileSize = false;
      for (const idx of indices) {
        const entry = rows[idx];
        if (entry && !entry.isDir && entry.size != null) {
          bytes += entry.size;
          hasFileSize = true;
        }
      }
      const parts = [`${indices.size.toLocaleString()} selected`];
      if (hasFileSize) parts.push(`Size: ${formatBytes(bytes)}`);
      return parts.join(', ');
    }
    return null;
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
        message: status.message,
        backgroundActive: status.backgroundActive ?? false
      };
      if (status.state === 'Indexing' && prevState !== 'Indexing') {
        startElapsedTimer();
      } else if (status.state !== 'Indexing' && prevState === 'Indexing') {
        stopElapsedTimer();
      }
      if (typeof status.scanned === 'number') {
        scanned = status.scanned;
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
    if (now - lastSearchFiredAt >= SEARCH_DEBOUNCE_MS) {
      // Leading edge: return results quickly while user is typing.
      lastSearchFiredAt = now;
      void runSearch(preserveScroll, false);
    }

    // Trailing edge: once input settles, run again with total-count enabled.
    searchTimer = setTimeout(() => {
      if (scheduledGen !== scheduleGeneration) {
        return;
      }
      lastSearchFiredAt = performance.now();
      void runSearch(preserveScroll, true);
    }, SEARCH_DEBOUNCE_MS);
  }

  async function runSearch(preserveScroll = false, includeTotal = true) {
    const viewportStart = startIndex;
    const viewportRows = visibleCount;
    const fetchLimit = computeSearchFetchLimit({
      preserveScroll,
      pageSize: PAGE_SIZE,
      loadedCount: results.length,
      viewportStart,
      viewportRows
    });
    if (preserveScroll && fetchLimit === null) {
      void runSearch(false, true);
      return;
    }

    searchGeneration += 1;
    const gen = searchGeneration;
    searchPending = true;
    const searchQuery = query;
    const searchSortBy = sortBy;
    const searchSortDir = sortDir;
    try {
      const removedAtStart = new Set(pendingTrashedPaths);
      const [next, stillPending] = await Promise.all([
        invoke('search', {
          query: searchQuery,
          limit: fetchLimit,
          offset: 0,
          sortBy: searchSortBy,
          sortDir: searchSortDir,
          includeTotal
        }),
        removedAtStart.size > 0
          ? invoke('get_pending_trash_paths', { paths: [...removedAtStart] }).catch(() => [...removedAtStart])
          : Promise.resolve([])
      ]);

      if (gen !== searchGeneration) return;

      dbLastQuery = searchQuery;
      const entries = Array.isArray(next.entries) ? next.entries : [];
      // Include the starting snapshot even if the watcher has just caught up:
      // this search may have read the DB before the deletion was committed.
      const visibleEntries = excludeTrashedEntries(entries, new Set([...removedAtStart, ...pendingTrashedPaths]), platform);
      searchModeLabel = next.modeLabel || '';

      if (preserveScroll && !shouldApplyPreserveResults(entries.length, viewportStart, viewportRows)) return;

      if (preserveScroll && tableContainer) scrollTop = getScrollEl().scrollTop;
      replaceResults(visibleEntries);
      loadedResultCount = entries.length;
      pendingTrashedPaths = new Set([...pendingTrashedPaths].filter((path) => !removedAtStart.has(path) || stillPending.includes(path)));
      if (next.totalKnown) {
        totalResults = Math.max(0, next.totalCount - (entries.length - visibleEntries.length));
        totalResultsKnown = true;
        totalResultsQuery = searchQuery;
      } else if (!(totalResultsKnown && totalResultsQuery === searchQuery)) {
        // Keep the last exact total while a countless refresh (e.g. the
        // index_updated leading edge) runs for the same query; the trailing
        // includeTotal search corrects it moments later.
        totalResults = visibleEntries.length;
        totalResultsKnown = false;
      }
      hasMore = totalResultsKnown ? results.length < totalResults : entries.length >= fetchLimit;
      if (!preserveScroll && tableContainer) getScrollEl().scrollTop = 0;

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
        offset: loadedResultCount,
        sortBy: sortBy,
        sortDir: sortDir,
        includeTotal: false
      });
      if (gen !== searchGeneration) return;
      const arr = Array.isArray(batch.entries) ? batch.entries : [];
      const visibleEntries = excludeTrashedEntries(arr, pendingTrashedPaths, platform);
      loadedResultCount += arr.length;
      if (arr.length > 0) {
        if (tableContainer) scrollTop = getScrollEl().scrollTop;
        results = [...results, ...visibleEntries];
        if (totalResultsKnown) totalResults = Math.max(0, totalResults - (arr.length - visibleEntries.length));
      }
      hasMore = totalResultsKnown ? results.length < totalResults : arr.length >= PAGE_SIZE;
    } catch (err) {
      showToast(`Failed to load more: ${String(err)}`);
    } finally {
      loadingMore = false;
    }
  }

  async function moveSelection(delta, withRange = false) {
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
    await tick();
    tableContainer?.querySelector(`[data-row-index="${next}"]`)?.focus({ preventScroll: true });
  }

  function handleHeaderSort(column) {
    if (sortBy === column) {
      sortDir = sortDir === 'asc' ? 'desc' : 'asc';
    } else {
      sortBy = column;
      sortDir = 'asc';
    }
    if (tableContainer) getScrollEl().scrollTop = 0;
    void runSearch(false, true);
  }

  function handleRowClick(event, index) {
    contextMenu.visible = false;
    event.currentTarget?.focus({ preventScroll: true });

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
    event.currentTarget?.focus({ preventScroll: true });

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
          singleSelection: selectedIndices.size === 1,
          singleIsDir: (selectedIndices.size === 1 && primaryEntry()?.isDir) || false
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
    const isDark = theme === 'dark';

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
    if (trashInFlight) return;
    const paths = selectedPaths();
    if (paths.length === 0) {
      return;
    }

    trashInFlight = true;
    try {
      trashItemCount = paths.length;
      // WKWebView does not implement window.confirm without a UI delegate.
      // A modal HTML dialog also keeps keyboard events out of the result list.
      const confirmed = await new Promise((resolve) => {
        resolveTrashDialog = resolve;
        trashDialogEl.returnValue = 'cancel';
        trashDialogEl.showModal();
      });
      if (!confirmed) return;

      const outcome = await invoke('move_to_trash', { paths });
      const removedPaths = outcome.removedPaths;
      if (removedPaths.length > 0) {
        pendingTrashedPaths = new Set([...pendingTrashedPaths, ...removedPaths]);
        // Discard searches/pages started before the filesystem operation.
        searchGeneration += 1;
        scheduleGeneration += 1;
        clearTimeout(searchTimer);
        searchPending = false;
        const entries = excludeTrashedEntries(results, removedPaths, platform);
        totalResults = Math.max(0, totalResults - (results.length - entries.length));
        replaceResults(entries);
        await tick();
        scrollTop = getScrollEl()?.scrollTop ?? 0;
        updateViewportHeight();
      }
      if (outcome.failures.length > 0) {
        const first = outcome.failures[0];
        showToast(`${removedPaths.length} removed, ${outcome.failures.length} failed: ${first.path}: ${first.error}`);
      } else {
        showToast(`Moved ${removedPaths.length} item(s) to Trash.`);
      }
      if (removedPaths.length > 0) await runSearch(true);
    } catch (err) {
      showToast(`Failed to move to Trash: ${String(err)}`);
    } finally {
      trashInFlight = false;
    }
  }

  function closeTrashDialog() {
    resolveTrashDialog?.(trashDialogEl.returnValue === 'trash');
    resolveTrashDialog = null;
  }

  async function resetIndex() {
    if (indexStatus.state === 'Indexing') {
      showToast('Indexing in progress. Please try again after it completes.');
      return;
    }

    results = [];
    totalResults = 0;
    totalResultsKnown = false;
    totalResultsQuery = null;
    clearSelection();
    scanned = 0;
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

  // --- macOS daemon-writer onboarding / auto-start (design §5.4, §5.2) ---

  // 1s FDA polling while the onboarding modal or the FDA banner waits for the
  // user to flip the System Settings toggle. Grants often apply live, but
  // macOS can require relaunching an already-running (previously denied)
  // process — System Settings itself prompts "quit & reopen" — so the modal
  // also offers an explicit restart button as the escape hatch.
  function startFdaPoll() {
    if (fdaPollTimer) return;
    fdaPollTimer = setInterval(async () => {
      const ok = await invoke('check_full_disk_access').catch(() => false);
      if (onboarding) onboarding = { ...onboarding, fdaOk: ok };
      if (ok) {
        if (showFdaBanner) {
          showFdaBanner = false;
          showToast('전체 디스크 접근이 허용되었습니다. 인덱서가 곧 반영합니다.');
        }
        if (!showOnboarding) stopFdaPoll();
      }
    }, 1000);
  }

  function stopFdaPoll() {
    if (fdaPollTimer) {
      clearInterval(fdaPollTimer);
      fdaPollTimer = null;
    }
  }

  // Records the first-run decision; the daemon's gate releases within ~5s and
  // the first registration starts it immediately (G3).
  async function decideOnboarding(decision) {
    if (onboardingBusy) return;
    onboardingBusy = true;
    try {
      await invoke('record_onboarding_decision', { decision });
      showOnboarding = false;
      stopFdaPoll();
      onboarding = await invoke('get_onboarding_state').catch(() => onboarding);
      if (decision === 'skipped') {
        showToast('제한된 인덱스로 시작합니다. 권한은 나중에 허용할 수 있습니다.');
        if (onboarding && !onboarding.fdaOk && localStorage.getItem(FDA_DISMISSED_KEY) !== '1') {
          showFdaBanner = true;
        }
      } else {
        showToast('백그라운드 인덱서를 시작합니다.');
      }
    } catch (err) {
      showToast(`온보딩 저장 실패: ${String(err)}`);
    } finally {
      onboardingBusy = false;
    }
  }

  $: autostartOn =
    onboarding && !onboarding.optedOut && onboarding.registration === 'enabled';

  async function toggleAutostart() {
    if (autostartBusy || !onboarding) return;
    autostartBusy = true;
    const enable = !autostartOn;
    try {
      await invoke('set_daemon_autostart', { enabled: enable });
      onboarding = await invoke('get_onboarding_state').catch(() => onboarding);
      showToast(
        enable
          ? '로그인 시 자동 실행이 켜졌습니다.'
          : '자동 실행이 꺼졌습니다. 앱을 사용하는 동안에만 인덱스가 갱신됩니다.'
      );
    } catch (err) {
      showToast(`자동 실행 설정 실패: ${String(err)}`);
    } finally {
      autostartBusy = false;
    }
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

    // Measure overlay position from the actual rendered DOM elements.
    const visibleIndex = idx - startIndex;
    const rowEl = tableContainer?.querySelectorAll('.row')?.[visibleIndex];
    const nameCell = rowEl?.querySelector('.cell.name');
    const span = nameCell?.querySelector('.ellipsis');
    if (rowEl && nameCell && span) {
      const rowRect = rowEl.getBoundingClientRect();
      const spanRect = span.getBoundingClientRect();
      const pathCell = rowEl.querySelector('.cell.path');
      const rightEdge = pathCell ? pathCell.getBoundingClientRect().right : nameCell.getBoundingClientRect().right;
      renameOverlay = {
        top: rowRect.top,
        left: spanRect.left - 9,
        maxWidth: rightEdge - spanRect.left + 5 - 8
      };
    }

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
    if (renameCommitting) return;
    if (!editing.active || editing.index < 0 || !results[editing.index]) {
      return;
    }

    renameCommitting = true;
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
    } finally {
      renameCommitting = false;
    }
  }

  function onGlobalClick() {
    closeContextMenu();
  }

  let snapTimer = null;
  let maximizeStateTimer = null;

  function refreshMaximizedStateSoon() {
    clearTimeout(maximizeStateTimer);
    maximizeStateTimer = setTimeout(() => {
      appWindow.isMaximized().then((value) => {
        isMaximized = value;
      }).catch(() => {});
    }, 80);
  }

  function snapLayoutEnter() {
    clearTimeout(snapTimer);
    clearTimeout(maximizeStateTimer);
    snapTimer = setTimeout(() => {
      appWindow
        .setFocus()
        .then(() => invoke('plugin:decorum|show_snap_overlay'))
        .catch(() => {});
    }, 620);
  }
  function snapLayoutLeave() {
    clearTimeout(snapTimer);
    clearTimeout(maximizeStateTimer);
  }

  function onTitleBarMouseDown(event) {
    if (event.button !== 0) {
      return;
    }

    const activeElement = document.activeElement;
    if (activeElement instanceof HTMLElement && isTextInputTarget(activeElement)) {
      activeElement.blur();
    }
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

  function handleCopy(event) {
    if (trashDialogEl?.open || isTextInputTarget(event.target) || selectedIndices.size === 0) return;
    event.preventDefault();
    void (platform === 'macos' ? copyFiles() : copySelectedPaths());
  }

  async function handleKeydown(event) {
    if (trashDialogEl?.open) return;
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
        const idx = editing.index;
        cancelRename();
        selectSingle(idx);
        await tick();
        tableContainer?.querySelector('.row.selected')?.focus();
        return;
      }
      return;
    }

    if (event.key === 'Escape') {
      event.preventDefault();
      clearSelection();
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

    if (event.key === 'Enter' && !event.metaKey && !event.ctrlKey) {
      event.preventDefault();
      await openSelected();
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
      if (platform === 'macos' && !event.altKey) {
        await copyFiles();
      } else {
        await copySelectedPaths();
      }
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

  function formatBytes(bytes) {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  }

  function formatSize(entry) {
    if (entry.isDir || entry.size == null) return '';
    return formatBytes(entry.size);
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
    clearInterval(statusRefreshTimer);
    clearInterval(bgPollTimer);
    indexingStartTime = Date.now();
    indexingElapsed = '0s';
    indexingFinishedAt = '';
    elapsedTimer = setInterval(updateElapsed, 1000);
    statusRefreshTimer = setInterval(() => { void refreshStatus(); }, 1000);
  }

  function stopElapsedTimer() {
    clearInterval(elapsedTimer);
    clearInterval(statusRefreshTimer);
    if (indexingStartTime) {
      const elapsed = Date.now() - indexingStartTime;
      indexingFinishedAt = `${formatElapsed(elapsed)}`;
    }
    indexingStartTime = null;
    indexingElapsed = '';
  }

  function startBackgroundPoll() {
    clearInterval(bgPollTimer);
    bgPollTimer = setInterval(async () => {
      try {
        const status = await invoke('get_index_status');
        indexStatus = {
          ...indexStatus,
          backgroundActive: status.backgroundActive ?? false,
          entriesCount: status.entriesCount ?? indexStatus.entriesCount
        };
        if (!status.backgroundActive) {
          clearInterval(bgPollTimer);
        }
      } catch (_) {
        clearInterval(bgPollTimer);
      }
    }, 1000);
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
  let onVisibilityChange = null;

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
    try {

    updateViewportHeight();
    startupLog(`[startup/fe] +${ms()}ms updateViewportHeight done`);

    invoke('set_native_theme', { theme });

    platform = await step('invoke(get_platform)', () => invoke('get_platform'));

    if (platform === 'macos') {
      onboarding = await invoke('get_onboarding_state').catch(() => null);
      if (onboarding && !onboarding.recorded) {
        // First run: the daemon idles in WaitingOnboarding (no build, no TCC
        // access) until a decision is recorded here (§5.4).
        showOnboarding = true;
        startFdaPoll();
      } else {
        const hasAccess = onboarding
          ? onboarding.fdaOk
          : await invoke('check_full_disk_access').catch(() => true);
        const dismissed = localStorage.getItem(FDA_DISMISSED_KEY) === '1';
        showFdaBanner = !hasAccess && !dismissed;
      }
    }

    let unlistenResized = null;
    if (platform === 'windows') {
      try {
        isMaximized = await appWindow.isMaximized().catch(() => false);
        unlistenResized = await appWindow.onResized(() => {
          refreshMaximizedStateSoon();
        });
      } catch (err) {
        console.warn('[startup/fe] windows resize hooks unavailable', err);
      }
    }

    const unlistenProgress = await step(
      'listen(index_progress)',
      () => listen('index_progress', (event) => {
        scanned = event.payload.scanned;
        indexed = event.payload.indexed;
        currentPath = event.payload.currentPath;
        if (indexStatus.state !== 'Indexing' && indexStatus.state !== 'Ready') {
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
          if (event.payload.state === 'Ready') startBackgroundPoll();
        }

        if (event.payload.state === 'Ready') {
          void refreshStatus();
          scheduleSearch(true);
        }

      })
    );

    const unlistenUpdated = await step(
      'listen(index_updated)',
      () => listen('index_updated', (event) => {
        const changed =
          event.payload.entriesCount !== indexStatus.entriesCount ||
          event.payload.lastUpdated !== indexStatus.lastUpdated;
        indexStatus = {
          ...indexStatus,
          entriesCount: event.payload.entriesCount,
          lastUpdated: event.payload.lastUpdated,
          permissionErrors: event.payload.permissionErrors ?? indexStatus.permissionErrors
        };
        if (changed) scheduleSearch(true);
      })
    );

    const unlistenCtxMenuAction = await step(
      'listen(context_menu_action)',
      () => listen('context_menu_action', (event) => {
        switch (event.payload) {
          case 'open': void openSelected(); break;
          case 'show_package_contents': invokeOnPrimary('show_package_contents'); break;
          case 'quick_look': invokeOnPrimary('quick_look'); break;
          case 'open_with': void openWithFallback(); break;
          case 'reveal': void revealSelected(); break;
          case 'copy_files': void copyFiles(); break;
          case 'copy_path': void copySelectedPaths(); break;
          case 'trash': void trashSelected(); break;
          case 'rename': void startRename(); break;
        }
      })
    );

    const unlistenPathignore = await step(
      'listen(pathignore_changed)',
      () => listen('pathignore_changed', () => {
        showPathignoreBanner = true;
      })
    );

    const unlistenPathindexing = await step(
      'listen(pathindexing_changed)',
      () => listen('pathindexing_changed', () => {
        showPathindexingBanner = true;
        scheduleSearch();
      })
    );

    // Windows: the read-only GUI polls the indexer service; this event carries
    // its health so a banner can appear when it's missing/stopped/broken (D5).
    const unlistenServiceHealth = await step(
      'listen(service_health)',
      () => listen('service_health', (event) => {
        serviceHealth = event.payload || { ok: true, state: '' };
      })
    );

    // Repaint watchdog (src-tauri/src/ui_gate.rs). The ack goes out from inside
    // a requestAnimationFrame on purpose: rAF only runs when the page actually
    // renders a frame, so it distinguishes a painting window from one whose JS
    // runs while WebKit keeps its layer tree hidden. Silence here is what tells
    // the backend to reload us.
    const unlistenRepaintProbe = await step(
      'listen(ui_repaint_probe)',
      () => listen('ui_repaint_probe', (event) => {
        const seq = event.payload;
        requestAnimationFrame(() => {
          void invoke('note_webview_painted', { seq }).catch(() => {});
        });
      })
    );

    unlistenFns = [unlistenProgress, unlistenState, unlistenUpdated, unlistenCtxMenuAction, unlistenPathignore, unlistenPathindexing, unlistenServiceHealth, unlistenRepaintProbe, unlistenResized].filter(Boolean);
    startupLog(`[startup/fe] +${ms()}ms all listeners registered`);

    // While we're hidden the backend holds index events rather than evaluating
    // JS into a webview macOS may have suspended, so it needs to know. Coming
    // back, the held events arrive as one flush and this catches the status line
    // up with it.
    const reportVisibility = () =>
      void invoke('set_webview_visible', { visible: !document.hidden }).catch(() => {});
    onVisibilityChange = () => {
      reportVisibility();
      if (!document.hidden) void refreshStatus();
    };
    document.addEventListener('visibilitychange', onVisibilityChange);
    reportVisibility();

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
    if (typeof ResizeObserver !== 'undefined') {
      tableResizeObserver = new ResizeObserver(() => {
        scheduleViewportHeightUpdate();
      });
      if (tableContainer) {
        tableResizeObserver.observe(tableContainer);
      } else if (tableAreaEl) {
        tableResizeObserver.observe(tableAreaEl);
      }
    }

    window.addEventListener('resize', scheduleViewportHeightUpdate);
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
        if (editing.active) void commitRename();

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

    // Virtual-scroll rows populated during startup can fail to paint in WebView2
    // until a reflow (exposed by the flex shell layout): the rows are in the DOM
    // and correctly sized, but not composited until the scroll offset changes.
    // Nudge OverlayScrollbars + the scroll position once the first frame settles
    // so the initial results are visible without requiring user interaction.
    requestAnimationFrame(() => {
      try { osInstance?.update?.(true); } catch {}
      updateViewportHeight();
      const el = getScrollEl();
      if (el) {
        el.scrollTop = 1;
        requestAnimationFrame(() => { el.scrollTop = 0; });
      }
    });

    startupLog(`[startup/fe] +${ms()}ms onMount complete`);
    if (DEBUG_STARTUP) console.log(`[startup/fe] +${ms()}ms onMount complete`);
    } catch (err) {
      console.error('[startup/fe] onMount failed', err);
      await tick().catch(() => {});
      document.getElementById('skeleton')?.remove();
      showToast(`Startup failed: ${String(err)}`);
    }
  });

  onDestroy(async () => {
    resolveTrashDialog?.(false);
    clearTimeout(searchTimer);
    clearTimeout(toastTimer);
    clearTimeout(snapTimer);
    clearTimeout(maximizeStateTimer);
    clearInterval(statusRefreshTimer);
    clearInterval(elapsedTimer);
    clearInterval(bgPollTimer);
    stopFdaPoll();
    resizeCleanup?.();

    if (onVisibilityChange) {
      document.removeEventListener('visibilitychange', onVisibilityChange);
      onVisibilityChange = null;
    }

    for (const unlisten of unlistenFns) {
      unlisten();
    }

    tableResizeObserver?.disconnect();
    tableResizeObserver = null;
    if (viewportResizeRaf !== null) {
      cancelAnimationFrame(viewportResizeRaf);
      viewportResizeRaf = null;
    }
    window.removeEventListener('resize', scheduleViewportHeightUpdate);
    window.removeEventListener('click', onGlobalClick);
    scrollCleanup?.();
    if (osInstance) {
      osInstance.destroy();
    }
  });
</script>

<svelte:window on:keydown={handleKeydown} on:copy={handleCopy} />

<div class="app-shell" data-theme={theme} data-platform={platform}>
  <div class="title-bar" role="presentation" data-tauri-drag-region on:mousedown={onTitleBarMouseDown}>
    <div class="title-left" class:win-title-left={platform === 'windows'}></div>
    <div class="title-center">
      <svg class="title-icon" viewBox="0 0 215 205" fill-rule="evenodd" aria-hidden="true">
        <path d="M85 0c46.9 0 85 38.1 85 85 0 20.1-7.2 38.5-19.2 52.7l40.7 40.8c5.8 5.8 5.8 15.2 0 21-5.8 5.8-15.2 5.8-21 0l-40.7-40.7c-13.6 9.7-30.4 15.2-44.8 15.2-46.9 0-85-42.1-85-89 0-46.9 38.1-85 85-85z m0 32c-29.3 0-53 23.7-53 53 0 29.3 23.7 53 53 53 29.3 0 53-23.7 53-53 0-29.3-23.7-53-53-53z"/>
      </svg>
      <span class="title-text">Everything</span>
    </div>
    {#if platform === 'windows'}
      <div class="win-controls">
        <button class="win-ctrl-btn" on:click={() => appWindow.minimize()} aria-label="Minimize">
          <svg viewBox="0 0 12 12" width="12" height="12"><line x1="1" y1="6" x2="11" y2="6" stroke="currentColor" stroke-width="1"/></svg>
        </button>
        <button class="win-ctrl-btn"
          on:click={() => appWindow.toggleMaximize()}
          on:mouseenter={snapLayoutEnter}
          on:mouseleave={snapLayoutLeave}
          aria-label={isMaximized ? 'Restore' : 'Maximize'}>
          {#if isMaximized}
            <svg viewBox="0 0 12 12" width="12" height="12"><rect x="2.5" y="3.5" width="6" height="6" fill="none" stroke="currentColor" stroke-width="1"/><polyline points="4.5,3.5 4.5,1.5 10.5,1.5 10.5,7.5 8.5,7.5" fill="none" stroke="currentColor" stroke-width="1"/></svg>
          {:else}
            <svg viewBox="0 0 12 12" width="12" height="12"><rect x="2" y="2" width="8" height="8" fill="none" stroke="currentColor" stroke-width="1"/></svg>
          {/if}
        </button>
        <button class="win-ctrl-btn win-ctrl-close" on:click={() => appWindow.close()} aria-label="Close">
          <svg viewBox="0 0 12 12" width="12" height="12"><line x1="2" y1="2" x2="10" y2="10" stroke="currentColor" stroke-width="1"/><line x1="10" y1="2" x2="2" y2="10" stroke="currentColor" stroke-width="1"/></svg>
        </button>
      </div>
    {:else}
      <div class="title-right"></div>
    {/if}
  </div>

  {#if showOnboarding}
  <div class="onboarding-overlay">
    <div class="onboarding-modal" role="dialog" aria-modal="true" aria-label="Everything 시작하기">
      <h2>Everything 시작하기</h2>
      <p>
        백그라운드 인덱서가 로그인할 때 자동으로 실행되어 파일 인덱스를 항상
        최신으로 유지합니다. 보호된 폴더(데스크탑·문서·다운로드 등)까지
        검색하려면 <b>전체 디스크 접근</b> 권한이 필요합니다.
      </p>
      <div class="onboarding-fda">
        {#if onboarding?.fdaOk}
          <span class="onboarding-fda-ok">✓ 전체 디스크 접근 허용됨</span>
        {:else}
          <span class="onboarding-fda-missing">전체 디스크 접근이 아직 허용되지 않았습니다</span>
          <button class="fda-btn-open" on:click={() => { invoke('open_privacy_settings'); startFdaPoll(); }}>설정 열기</button>
          <button class="fda-btn-open" on:click={() => invoke('restart_app')}>앱 재시작</button>
        {/if}
      </div>
      <p class="onboarding-note">
        시스템 설정 &gt; 개인정보 보호 및 보안 &gt; 전체 디스크 접근에서 Everything을
        켜 주세요. 목록에 없으면 + 버튼으로 응용 프로그램에서 추가할 수 있습니다.
        허용하면 이 창이 자동으로 감지합니다 — 잠시 후에도 감지되지 않으면
        (시스템이 재실행을 요구하는 경우) '앱 재시작'을 눌러 주세요.
      </p>
      <div class="onboarding-actions">
        <button
          class="onboarding-primary"
          disabled={onboardingBusy || !onboarding?.fdaOk}
          on:click={() => decideOnboarding('granted')}
        >
          시작하기
        </button>
        <button
          class="onboarding-secondary"
          disabled={onboardingBusy}
          on:click={() => decideOnboarding('skipped')}
        >
          나중에 (제한된 인덱스로 시작)
        </button>
      </div>
    </div>
  </div>
  {/if}

  {#if showFdaBanner}
  <div class="fda-banner" role="alert">
    {#if fdaSettingsOpened}
      <span class="fda-banner-text">전체 디스크 접근을 허용하면 자동으로 반영됩니다 (인덱서가 1분 내 감지).</span>
    {:else}
      <span class="fda-banner-text">전체 디스크 접근을 허용하면 보호된 폴더까지 검색할 수 있습니다.</span>
    {/if}
    <div class="fda-banner-actions">
      <button class="fda-btn-open" on:click={() => { invoke('open_privacy_settings'); fdaSettingsOpened = true; startFdaPoll(); }}>설정 열기</button>
      <button class="fda-btn-dismiss" on:click={() => { showFdaBanner = false; stopFdaPoll(); localStorage.setItem(FDA_DISMISSED_KEY, '1'); }}>닫기</button>
    </div>
  </div>
  {/if}

  {#if showPathignoreBanner}
  <div class="fda-banner" role="alert">
    <span class="fda-banner-text">설정 파일(.pathignore)이 변경되었습니다. 재시작 후 적용됩니다.</span>
    <div class="fda-banner-actions">
      <button class="fda-btn-open" on:click={() => invoke('restart_app')}>재시작</button>
      <button class="fda-btn-dismiss" on:click={() => showPathignoreBanner = false}>닫기</button>
    </div>
  </div>
  {/if}

  {#if showPathindexingBanner}
  <div class="fda-banner" role="alert">
    <span class="fda-banner-text">인덱싱 경로(.pathindexing)가 변경되어 반영되었습니다.</span>
    <div class="fda-banner-actions">
      <button class="fda-btn-dismiss" on:click={() => showPathindexingBanner = false}>닫기</button>
    </div>
  </div>
  {/if}

  {#if !serviceHealth.ok}
  <div class="fda-banner" role="alert">
    <span class="fda-banner-text">
      {#if serviceHealth.state === 'not_installed'}
        인덱스 서비스가 설치되어 있지 않습니다. 등록하려면 관리자 권한이 필요합니다.
      {:else if serviceHealth.state === 'stopped' || serviceHealth.state === 'starting'}
        인덱스 서비스를 시작하는 중입니다...
      {:else if serviceHealth.state === 'unresponsive'}
        인덱스 서비스가 응답하지 않습니다. 재등록이 필요할 수 있습니다.
      {:else if serviceHealth.state === 'version_mismatch'}
        인덱스 서비스가 이전 버전으로 실행 중입니다. 재시작(재등록)이 필요합니다.
      {:else if serviceHealth.state === 'not_registered'}
        백그라운드 인덱서가 로그인 항목으로 등록되어 있지 않습니다.
      {:else if serviceHealth.state === 'approval_required'}
        로그인 항목에서 Everything이 꺼져 있어 백그라운드 인덱서가 실행되지 않습니다.
      {:else if serviceHealth.state === 'disabled'}
        자동 실행이 꺼져 있습니다. 앱을 사용하는 동안에만 인덱스가 갱신됩니다.
      {:else}
        인덱스 서비스 상태를 확인하는 중입니다...
      {/if}
    </span>
    <div class="fda-banner-actions">
      {#if serviceHealth.state === 'not_installed' || serviceHealth.state === 'unresponsive' || serviceHealth.state === 'version_mismatch'}
        {#if platform === 'windows'}
          <button class="fda-btn-open" on:click={() => invoke('register_service_elevated')}>서비스 등록</button>
        {/if}
      {/if}
      {#if serviceHealth.state === 'not_registered'}
        <button class="fda-btn-open" on:click={() => invoke('register_daemon').then(() => showToast('로그인 항목으로 등록했습니다.')).catch((e) => showToast(String(e)))}>등록</button>
      {/if}
      {#if serviceHealth.state === 'approval_required'}
        <button class="fda-btn-open" on:click={() => invoke('open_login_items_settings')}>로그인 항목 열기</button>
      {/if}
      {#if serviceHealth.state === 'disabled'}
        <button class="fda-btn-open" disabled={autostartBusy} on:click={toggleAutostart}>다시 켜기</button>
      {/if}
    </div>
  </div>
  {/if}

  <header class="search-bar">
    <input
      bind:this={searchInputEl}
      class="search-input"
      type="text"
      bind:value={query}
      on:input={() => { indexingFinishedAt = ''; scheduleSearch(); }}
      on:focus={clearSelection}
      placeholder="Search file/folder names"
      aria-label="Search file and folder names"
      autocomplete="off"
      spellcheck="false"
    />
  </header>

  <section class="table-area" bind:this={tableAreaEl} style={tableGridStyle} role="table" aria-label="Search results">
    <div class="table-header" role="rowgroup">
      <div class="table-header-track">
        <div class="col name">
          <button type="button" class="col-button" on:click={() => handleHeaderSort('name')}>
            Name{#if sortBy === 'name'}{@html sortIconHtml(sortDir)}{/if}
          </button>
          <button
            type="button"
            class="col-resizer"
            class:active={resizingColumn === 'name'}
            on:mousedown={(event) => startColumnResize(event, 'name')}
            aria-label="Resize Name column"
          ></button>
        </div>

        <div class="col path">
          <button type="button" class="col-button" on:click={() => handleHeaderSort('dir')}>
            Path{#if sortBy === 'dir'}{@html sortIconHtml(sortDir)}{/if}
          </button>
          <button
            type="button"
            class="col-resizer"
            class:active={resizingColumn === 'path'}
            on:mousedown={(event) => startColumnResize(event, 'path')}
            aria-label="Resize Path column"
          ></button>
        </div>

        <div class="col size">
          <button type="button" class="col-button" on:click={() => handleHeaderSort('size')}>
            Size{#if sortBy === 'size'}{@html sortIconHtml(sortDir)}{/if}
          </button>
        </div>

        <div class="col modified">
          <button type="button" class="col-button" on:click={() => handleHeaderSort('mtime')}>
            Modified{#if sortBy === 'mtime'}{@html sortIconHtml(sortDir)}{/if}
          </button>
        </div>
      </div>
    </div>

    <div
      class="table-body"
      bind:this={tableContainer}
      role="rowgroup"
    >
      <div class="spacer" style={`height:${totalHeight}px`}>
        <div class="rows" style={`transform: translateY(${translateY}px);`}>
          {#each visibleRows as entry, localIndex}
            {@const index = startIndex + localIndex}
            <div
              class="row {selectedIndices.has(index) ? 'selected' : ''}"
              data-row-index={index}
              on:mousedown={(event) => handleRowMouseDown(event, index)}
              on:click={(event) => handleRowClick(event, index)}
              on:keydown={(event) => handleRowA11yKeydown(event, index)}
              on:dblclick={() => handleRowDoubleClick(index)}
              on:contextmenu={(event) => handleRowContextMenu(event, index)}
              role="row"
              aria-selected={selectedIndices.has(index)}
              tabindex="0"
            >
              <div class="cell name">
                <img class="file-icon" src={iconFor(entry, iconVersion)} alt="" draggable="false" />
                <span class="ellipsis" class:name-editing={editing.active && editing.index === index}>{#each highlightSegments(entry.name, query) as seg}{#if seg.hl}<mark class="hl">{seg.text}</mark>{:else}{seg.text}{/if}{/each}</span>
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

  <footer class="status-bar" role="status" aria-live="polite">
    <div class="status-left">
      {#if selectionInfo}
        <span class="status-selection ellipsis" title={selectionInfo}>{selectionInfo}</span>
      {:else}
        <span class="status-state">
          <span class="state-dot {indexStatus.state === 'Indexing' ? 'indexing' : indexStatus.state === 'Error' ? 'error' : 'ready'}" class:pulsing={indexStatus.state === 'Ready' && indexStatus.backgroundActive}></span>
          {#if indexStatus.state === 'Indexing'}
            Indexing{#if lastReadyCount > 0} ({Math.min(99, Math.round((scanned / lastReadyCount) * 100))}%){/if}{#if indexingElapsed} · {indexingElapsed}{/if}
            {#if !indexStatus.isCatchup}
              · {scanned.toLocaleString()} scanned
            {/if}
          {:else}
            Index: {indexStatus.state}
          {/if}
        </span>
        {#if indexingFinishedAt && indexStatus.state !== 'Indexing'}
          <span>in {indexingFinishedAt}</span>
        {/if}
        {#if dbLastQuery}
          <span>"{dbLastQuery}" · {totalResults} results</span>
        {/if}
      {/if}
      {#if searchModeLabel === 'spotlight' || searchModeLabel === 'spotlight_timeout'}
        <span class="status-spotlight">Spotlight fallback{#if searchModeLabel === 'spotlight_timeout'} (partial){/if}</span>
      {/if}
      <!-- The failure count used to render here as a bare "N permission errors"
           next to the backend's own message about the same N — two spans, no
           explanation. The message now carries both the number and the reason.
           Severity comes from the index state, not from the count: a message on
           a pass that completed is a caveat (some paths were unreadable), while
           only an actual Error state is an error. Keying off permissionErrors
           instead would mis-style both ways — it is sticky from the last pass,
           so a later watcher failure would read as a caveat, and an I/O-fault
           pass would too. -->
      {#if indexStatus.message}
        <span class={indexStatus.isCatchup ? 'index-progress' : indexStatus.state === 'Error' ? 'status-error' : 'status-warning'}>{indexStatus.message}</span>
      {/if}
    </div>
    <div class="status-right">
      {#if platform === 'macos' && onboarding}
        <button
          class="status-btn"
          on:click={toggleAutostart}
          disabled={autostartBusy || onboarding.registration === 'unsupported'}
          title={onboarding.registration === 'unsupported'
            ? '개발 모드에서는 로그인 항목 등록을 사용할 수 없습니다.'
            : '로그인 시 백그라운드 인덱서 자동 실행을 켜거나 끕니다.'}
        >
          자동 실행: {autostartOn ? '켜짐' : '꺼짐'}
        </button>
      {/if}
      <button class="status-btn" on:click={() => invoke('open_pathindexing')} title="추가 인덱싱 경로(.pathindexing) 열기">
        Add
      </button>
      <button class="status-btn" on:click={() => invoke('open_pathignore')} title="설정 파일(.pathignore) 열기">
        Ignore
      </button>
      <button
        class="status-btn rebuild-btn"
        on:click={resetIndex}
        disabled={indexStatus.state === 'Indexing'}
        title={indexStatus.state === 'Indexing' ? 'Cannot reset while indexing is in progress.' : 'Reset and rebuild the index.'}
      >
        <svg class="rebuild-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M23 4v6h-6"/><path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/>
        </svg>
        Reset
      </button>
    </div>
  </footer>

  {#if editing.active}
    <div
      class="rename-overlay"
      style="top:{renameOverlay.top}px; left:{renameOverlay.left}px; max-width:{renameOverlay.maxWidth}px;"
      role="presentation"
      on:mousedown|stopPropagation
    >
      <input
        type="text"
        bind:this={renameInputEl}
        class="rename-input"
        bind:value={editing.draftName}
        size={Math.max(1, editing.draftName.length)}
        on:click|stopPropagation
        on:blur={commitRename}
      />
    </div>
  {/if}

  {#if contextMenu.visible}
    <div class="context-menu" style={`left:${contextMenu.x}px;top:${contextMenu.y}px;`}>
      <button on:click={() => (closeContextMenu(), openSelected())}>Open</button>
      <button on:click={() => { closeContextMenu(); invokeOnPrimary('quick_look'); }}>Quick Look</button>
      <button on:click={() => (closeContextMenu(), openWithFallback())}>Open With... (Reveal in Finder)</button>
      <button on:click={() => (closeContextMenu(), revealSelected())}>Reveal in Finder</button>
      <button on:click={() => (closeContextMenu(), copySelectedPaths())}>Copy Path</button>
      <button on:click={() => (closeContextMenu(), trashSelected())}>Move to Trash</button>
      {#if selectedIndices.size === 1}
        <button on:click={() => (closeContextMenu(), startRename())}>Rename</button>
      {/if}
    </div>
  {/if}

  <dialog class="trash-dialog" bind:this={trashDialogEl} on:close={closeTrashDialog} aria-labelledby="trash-dialog-title">
    <form method="dialog">
      <h2 id="trash-dialog-title">{trashItemCount === 1 ? 'Move selected item to Trash?' : `Move ${trashItemCount} items to Trash?`}</h2>
      <div class="trash-dialog-actions">
        <button value="cancel">Cancel</button>
        <button value="trash">Move to Trash</button>
      </div>
    </form>
  </dialog>

  {#if toast}
    <div class="toast">{toast}</div>
  {/if}
</div>

<style>
  .trash-dialog {
    max-width: min(360px, calc(100vw - 48px));
    padding: 24px;
    border: 1px solid var(--menu-border);
    border-radius: 12px;
    background: var(--menu-bg);
    color: var(--text-primary);
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.25);
  }

  .trash-dialog::backdrop {
    background: rgba(0, 0, 0, 0.25);
  }

  .trash-dialog h2 {
    margin: 0 0 24px;
    font-size: 14px;
    font-weight: 600;
  }

  .trash-dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .trash-dialog-actions button {
    padding: 6px 12px;
    border: 1px solid var(--button-border);
    border-radius: 6px;
    background: var(--button-bg);
    color: var(--button-text);
    cursor: pointer;
  }

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

  /* Theme: Dark */
  :global([data-theme="dark"]) {
    color-scheme: dark;
    --bg-app: #1E1E21;
    --text-primary: #B8BDC5;
    --text-muted: #94A3B8;
    --bar-grad-top: #1E1E21;
    --bar-grad-bottom: #1E1E21;
    --surface-header: #1E1E21;
    --surface: #1E1E21;
    --bg-input: #161618;
    --border-soft: #2A2A2D;
    --border-input: rgba(255, 255, 255, 0.06);
    --focus-ring: #4DA8FF;
    --accent: #4DA8FF;
    --row-border: rgba(255, 255, 255, 0.02);
    --row-hover: #282828;
    --row-selected: rgba(77, 168, 255, 0.10);
    --button-bg: rgba(255, 255, 255, 0.05);
    --button-border: #2A2A2D;
    --button-text: #94A3B8;
    --menu-bg: #252528;
    --menu-border: #2A2A2D;
    --menu-hover: rgba(255, 255, 255, 0.04);
    --menu-text: #E2E8F0;
    --error-text: #ff9d9d;
    --warning-text: #e0c670;
    --toast-bg: rgba(10, 10, 16, 0.93);
    --toast-text: #f2f2f6;
  }

  /* Theme: Light */
  :global([data-theme="light"]) {
    color-scheme: light;
    --bg-app: #F5F5F7;
    --text-primary: #1C1C1E;
    --text-muted: #6E6E7A;
    --bar-grad-top: #F5F5F7;
    --bar-grad-bottom: #F5F5F7;
    --surface-header: #F5F5F7;
    --surface: #F5F5F7;
    --bg-input: #FFFFFF;
    --border-soft: #D1D1D6;
    --border-input: rgba(0, 0, 0, 0.10);
    --focus-ring: #3A8BF5;
    --accent: #3A8BF5;
    --row-border: rgba(0, 0, 0, 0.04);
    --row-hover: rgba(0, 0, 0, 0.04);
    --row-selected: rgba(58, 139, 245, 0.10);
    --button-bg: #FFFFFF;
    --button-border: #D1D1D6;
    --button-text: #3C3C43;
    --menu-bg: #FFFFFF;
    --menu-border: #D1D1D6;
    --menu-hover: rgba(0, 0, 0, 0.05);
    --menu-text: #1C1C1E;
    --error-text: #D93025;
    --warning-text: #B45309;
    --toast-bg: rgba(255, 255, 255, 0.95);
    --toast-text: #1C1C1E;
  }

  /* Themed element overrides */
  :global([data-theme]) {
    background: var(--bg-app);
  }

  /* Search bar: flat, no gradient, no separator */
  :global([data-theme]) .search-bar {
    height: 44px;
    padding: 0 16px;
    background: var(--bg-app);
    border-bottom: none;
  }

  :global([data-theme]) .search-input {
    backdrop-filter: none;
    -webkit-backdrop-filter: none;
    font-family: 'JetBrains Mono', 'SF Mono', monospace;
    font-size: 12px;
    height: 34px;
    border-radius: 8px;
  }

  /* Table header: flat bg, 16px edge padding */
  :global([data-theme]) .table-header {
    background: var(--bg-app);
  }

  :global([data-theme]) .col-button {
    padding: 0 8px;
  }

  :global([data-theme]) .col.name .col-button {
    padding-left: 16px;
  }

  /* Row cells: JetBrains Mono, 16px outer edges */
  :global([data-theme]) .cell {
    font-family: 'JetBrains Mono', monospace;
  }

  :global([data-theme]) .cell.name {
    font-family: Inter, 'SF Pro Text', sans-serif;
    padding-left: 16px;
    font-size: 12px;
    color: var(--text-primary);
  }

  :global([data-theme]) .cell.path {
    font-size: 11px;
    padding-right: 4px;
  }

  :global([data-theme]) .cell.size {
    font-size: 11px;
  }

  :global([data-theme]) .cell.modified {
    padding-right: 16px;
    font-size: 11px;
    white-space: nowrap;
  }

  :global([data-theme]) .row.selected {
    box-shadow: inset 3px 0 0 var(--accent);
  }

  /* Dim path/size/date cells in themed mode */
  :global([data-theme]) .cell.path,
  :global([data-theme]) .cell.size,
  :global([data-theme]) .cell.modified {
    color: var(--text-muted);
  }

  :global(:root) {
    background: transparent;
  }

  :global(html, body) {
    margin: 0;
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: transparent;
    color: var(--text-primary);
    font-family: Inter, 'SF Pro Text', 'Segoe UI', Helvetica, Arial, sans-serif;
  }

  :global(#app) {
    width: 100%;
    height: 100%;
  }

  .app-shell {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-width: 0;
  }

  .title-bar {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 44px;
    padding: 0 16px;
    background: var(--surface-header);
    cursor: default;
    flex-shrink: 0;
    user-select: none;
    -webkit-user-select: none;
  }

  .title-left {
    width: 72px;
    pointer-events: none;
    user-select: none;
    -webkit-user-select: none;
  }

  .win-title-left {
    width: 8px;
  }

  .title-center {
    position: absolute;
    left: 50%;
    top: 50%;
    transform: translate(-50%, -50%);
    display: flex;
    align-items: center;
    gap: 8px;
    pointer-events: none;
    user-select: none;
    -webkit-user-select: none;
  }

  .title-right {
    width: 72px;
    pointer-events: none;
    user-select: none;
    -webkit-user-select: none;
  }

  .win-controls {
    display: flex;
    align-items: stretch;
    height: 100%;
    margin-right: -16px;
  }

  .win-ctrl-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 46px;
    height: 100%;
    border: none;
    padding: 0;
    background: transparent;
    color: var(--text-primary);
    cursor: default;
    outline: none;
    -webkit-app-region: no-drag;
  }

  .win-ctrl-btn:hover {
    background: var(--row-hover);
  }

  .win-ctrl-close:hover {
    background: #e81123;
    color: #fff;
  }

  .title-icon {
    width: 14px;
    height: 14px;
    fill: var(--accent);
    flex-shrink: 0;
  }

  .title-text {
    font-family: Inter, 'SF Pro Text', sans-serif;
    font-size: 13px;
    line-height: 1;
    font-weight: 500;
    color: var(--text-primary);
    letter-spacing: -0.01em;
  }

  .fda-banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 7px 12px;
    background: #e8a020;
    flex-shrink: 0;
    min-width: 0;
  }
  :global([data-theme='dark']) .fda-banner {
    background: #7a4800;
  }
  .fda-banner-text {
    font-size: 11.5px;
    color: #fff;
    flex: 1 1 auto;
    min-width: 0;
    line-height: 1.4;
  }
  .fda-banner-actions {
    display: flex;
    gap: 6px;
    flex-shrink: 0;
  }
  .fda-btn-open, .fda-btn-dismiss {
    padding: 3px 10px;
    border-radius: 5px;
    font-size: 11.5px;
    font-weight: 500;
    cursor: pointer;
    border: none;
    white-space: nowrap;
  }
  .fda-btn-open {
    background: #fff;
    color: #a05000;
  }
  .fda-btn-dismiss {
    background: transparent;
    color: rgba(255,255,255,0.85);
  }
  .fda-btn-dismiss:hover {
    color: #fff;
  }

  .onboarding-overlay {
    position: fixed;
    inset: 0;
    z-index: 60;
    background: rgba(0, 0, 0, 0.45);
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .onboarding-modal {
    width: 440px;
    max-width: calc(100vw - 48px);
    background: var(--bar-grad-bottom, #f2f2f2);
    border: 1px solid var(--border-soft, #d0d0d0);
    border-radius: 12px;
    padding: 18px 20px 16px;
    box-shadow: 0 16px 48px rgba(0, 0, 0, 0.35);
    font-size: 12.5px;
    line-height: 1.65;
  }
  .onboarding-modal h2 {
    margin: 0 0 8px;
    font-size: 15px;
  }
  .onboarding-modal p {
    margin: 0 0 10px;
  }
  .onboarding-note {
    font-size: 11.5px;
    opacity: 0.75;
  }
  .onboarding-fda {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 8px 10px;
    border-radius: 8px;
    background: rgba(232, 160, 32, 0.18);
    margin-bottom: 10px;
  }
  .onboarding-fda-ok {
    color: #1f9d57;
    font-weight: 600;
  }
  .onboarding-fda-missing {
    font-weight: 500;
  }
  .onboarding-fda .fda-btn-open {
    background: #e8a020;
    color: #fff;
  }
  .onboarding-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    margin-top: 4px;
  }
  .onboarding-primary,
  .onboarding-secondary {
    padding: 5px 14px;
    border-radius: 6px;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
    border: 1px solid transparent;
  }
  .onboarding-primary {
    background: #2f6ede;
    color: #fff;
  }
  .onboarding-primary:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .onboarding-secondary {
    background: transparent;
    border-color: var(--border-soft, #c0c0c0);
    color: inherit;
  }
  .onboarding-secondary:disabled {
    opacity: 0.45;
    cursor: default;
  }
  :global([data-theme='dark']) .onboarding-modal {
    background: #26292e;
    border-color: #3a3f46;
  }
  :global([data-theme='dark']) .onboarding-fda {
    background: rgba(232, 160, 32, 0.22);
  }

  .search-bar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 8px;
    background: linear-gradient(180deg, var(--bar-grad-top) 0%, var(--bar-grad-bottom) 100%);
    border-bottom: 1px solid var(--border-soft);
    min-width: 0;
    flex-shrink: 0;
  }

  .search-input {
    display: block;
    box-sizing: border-box;
    width: 100%;
    min-width: 0;
    flex: 1 1 auto;
    height: 36px;
    border: 1px solid var(--border-input);
    border-radius: 14px;
    padding: 0 10px;
    font-size: 14px;
    background: var(--bg-input, var(--surface));
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
    flex: 1;
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
    height: 32px;
    border-bottom: 1px solid var(--border-soft);
    background: var(--surface-header);
    user-select: none;
    overflow: hidden;
    flex-shrink: 0;
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
    gap: 4px;
    box-sizing: border-box;
    padding: 0 8px;
    font-family: Inter, 'SF Pro Text', sans-serif;
    font-size: 11px;
    font-weight: 600;
    line-height: 1;
    color: var(--text-muted);
    text-align: left;
    margin: 0;
    border: none;
    background: transparent;
    cursor: pointer;
  }

  .col.size .col-button {
    justify-content: flex-end;
  }

  :global(.sort-icon) {
    width: 10px;
    height: 10px;
    flex-shrink: 0;
    color: var(--accent);
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
    height: 26px;
    border-bottom: 1px solid var(--row-border);
    cursor: default;
    -webkit-user-select: none;
    user-select: none;
    -webkit-user-drag: none;
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
    min-width: 0;
  }

  .cell.size {
    padding: 0;
    justify-content: flex-end;
    white-space: nowrap;
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
    -webkit-user-drag: none;
    user-drag: none;
    pointer-events: none;
  }

  .name-editing {
    opacity: 0.25;
  }

  .rename-overlay {
    position: fixed;
    z-index: 100;
  }

  .rename-input {
    display: block;
    box-sizing: border-box;
    min-width: 120px;
    width: auto;
    font-family: Inter, 'SF Pro Text', sans-serif;
    font-size: 12px;
    line-height: 18px;
    height: 26px;
    padding: 0 4px;
    border: 1.5px solid var(--focus-ring);
    border-radius: 4px;
    background: var(--bg-input, var(--surface));
    color: var(--text-primary);
    outline: none;
    box-shadow: 0 2px 14px rgba(0, 0, 0, 0.22), 0 0 0 3px rgba(77, 168, 255, 0.13);
  }

  .status-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 32px;
    padding: 0 16px;
    background: var(--surface-header);
    border-top: 1px solid var(--border-soft);
    font-family: 'JetBrains Mono', monospace;
    font-size: 11px;
    color: var(--text-muted);
    white-space: nowrap;
    overflow: hidden;
    min-width: 0;
    flex-shrink: 0;
  }

  .status-left {
    display: flex;
    align-items: center;
    gap: 16px;
    overflow: hidden;
    min-width: 0;
  }

  .status-selection {
    min-width: 0;
  }

  .status-right {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }

  .status-btn {
    display: inline-flex;
    align-items: center;
    gap: 3px;
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

  .status-state {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .state-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .state-dot.ready { background: #28C840; }
  .state-dot.indexing { background: #FEBC2E; animation: pulse-dot 1.4s ease-in-out infinite; }
  .state-dot.error { background: #FF5F57; }

  .state-dot.pulsing {
    animation: pulse-dot 1.4s ease-in-out infinite;
  }

  @keyframes pulse-dot {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.25; }
  }

  .rebuild-btn {
    display: flex;
    align-items: center;
    gap: 6px;
    font-family: Inter, 'SF Pro Text', sans-serif;
    font-size: 10px;
    font-weight: 500;
    border-radius: 4px;
    padding: 0 10px;
  }

  .rebuild-btn:hover {
    filter: brightness(1.1);
  }

  .rebuild-btn:disabled {
    opacity: 0.5;
  }

  .rebuild-icon {
    width: 11px;
    height: 11px;
    color: var(--accent);
    flex-shrink: 0;
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

  /* Compact titlebar design (Windows + macOS overlay) */
  :global([data-platform='windows']) .title-bar,
  :global([data-platform='macos']) .title-bar {
    height: 30px;
  }
  :global([data-platform='windows']) .title-icon,
  :global([data-platform='macos']) .title-icon {
    width: 12px;
    height: 12px;
  }
  :global([data-platform='windows']) .title-text,
  :global([data-platform='macos']) .title-text {
    font-size: 12px;
  }
  :global([data-platform='macos']) .title-left {
    width: 72px;
  }
  :global([data-platform='windows']) .search-bar {
    padding: 0 8px 0;
  }
  :global([data-platform='windows']) .search-input {
    height: 30px;
    border-radius: 8px;
    font-size: 13px;
  }
  :global([data-platform='windows']) .table-header {
    height: 24px;
  }
  :global([data-platform='windows']) .col-resizer::after {
    top: 4px;
    bottom: 4px;
  }
</style>
