<script>
  import { onDestroy, onMount, tick } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';

  const rowHeight = 28;
  const defaultLimit = 300;

  let query = '';
  let results = [];
  let selectedIndices = new Set();
  let lastSelectedIndex = -1;
  let editing = {
    active: false,
    path: '',
    index: -1,
    draftName: ''
  };

  let indexStatus = {
    state: 'Ready',
    entriesCount: 0,
    lastUpdated: null,
    permissionErrors: 0,
    message: null
  };

  let sortBy = 'name';
  let sortDir = 'asc';

  let scanned = 0;
  let indexed = 0;
  let currentPath = '';

  let searchInputEl;
  let renameInputEl;
  let tableContainer;
  let scrollTop = 0;
  let viewportHeight = 520;

  let contextMenu = {
    visible: false,
    x: 0,
    y: 0
  };

  let toast = '';
  let searchTimer;
  let toastTimer;

  const iconCache = new Map();
  const iconLoading = new Set();

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
  $: startIndex = Math.max(0, Math.floor(scrollTop / rowHeight) - 6);
  $: visibleCount = Math.ceil(viewportHeight / rowHeight) + 12;
  $: endIndex = Math.min(results.length, startIndex + visibleCount);
  $: visibleRows = results.slice(startIndex, endIndex);
  $: translateY = startIndex * rowHeight;

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

  function iconKey(entry) {
    if (entry.isDir) {
      return '__folder__';
    }
    return entry.ext || '__file__';
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
      const bytes = await invoke('get_file_icon', { ext: entry.isDir ? 'folder' : entry.ext || '' });
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

  function updateViewportHeight() {
    viewportHeight = tableContainer?.clientHeight || 520;
  }

  function selectedPaths() {
    const sorted = [...selectedIndices].sort((a, b) => a - b);
    return sorted.map((idx) => results[idx]).filter(Boolean).map((entry) => entry.path);
  }

  function clearSelection() {
    selectedIndices = new Set();
    lastSelectedIndex = -1;
  }

  function selectSingle(index) {
    selectedIndices = new Set([index]);
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
    lastSelectedIndex = index;
  }

  function selectRange(index) {
    const anchor = lastSelectedIndex >= 0 ? lastSelectedIndex : index;
    const [from, to] = [Math.min(anchor, index), Math.max(anchor, index)];
    const next = new Set(selectedIndices);
    for (let i = from; i <= to; i += 1) {
      next.add(i);
    }
    selectedIndices = next;
  }

  function primaryIndex() {
    if (selectedIndices.size === 0) {
      return -1;
    }
    return Math.min(...selectedIndices);
  }

  function primaryEntry() {
    const idx = primaryIndex();
    return idx >= 0 ? results[idx] : null;
  }

  async function refreshStatus() {
    try {
      const status = await invoke('get_index_status');
      indexStatus = {
        state: status.state,
        entriesCount: status.entriesCount,
        lastUpdated: status.lastUpdated,
        permissionErrors: status.permissionErrors ?? 0,
        message: status.message
      };
    } catch (err) {
      showToast(`상태 조회 실패: ${String(err)}`);
    }
  }

  function scheduleSearch() {
    clearTimeout(searchTimer);
    const delay = query.trim().length <= 1 ? 50 : 0;
    searchTimer = setTimeout(() => {
      void runSearch();
    }, delay);
  }

  async function runSearch() {
    try {
      const keepPaths = new Set(selectedPaths());
      const next = await invoke('search', {
        query,
        limit: defaultLimit,
        sort_by: sortBy,
        sort_dir: sortDir
      });

      results = Array.isArray(next) ? next : [];

      const restored = new Set();
      for (let i = 0; i < results.length; i += 1) {
        if (keepPaths.has(results[i].path)) {
          restored.add(i);
        }
      }
      selectedIndices = restored;
      if (selectedIndices.size === 0 && results.length > 0 && query.trim().length > 0) {
        selectedIndices = new Set([0]);
        lastSelectedIndex = 0;
      }

      updateViewportHeight();
    } catch (err) {
      showToast(`검색 실패: ${String(err)}`);
    }
  }

  function moveSelection(delta, withRange = false) {
    if (results.length === 0) {
      return;
    }

    const current = primaryIndex() >= 0 ? primaryIndex() : 0;
    const next = Math.max(0, Math.min(results.length - 1, current + delta));

    if (withRange) {
      selectRange(next);
    } else {
      selectSingle(next);
    }

    const top = next * rowHeight;
    const bottom = top + rowHeight;
    if (top < scrollTop) {
      tableContainer.scrollTop = top;
    } else if (bottom > scrollTop + viewportHeight) {
      tableContainer.scrollTop = bottom - viewportHeight;
    }
  }

  function handleHeaderSort(column) {
    if (sortBy === column) {
      sortDir = sortDir === 'asc' ? 'desc' : 'asc';
    } else {
      sortBy = column;
      sortDir = 'asc';
    }
    void runSearch();
  }

  function sortMark(column) {
    if (sortBy !== column) {
      return '';
    }
    return sortDir === 'asc' ? ' ▲' : ' ▼';
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

  async function handleRowDoubleClick(index) {
    if (!selectedIndices.has(index) || selectedIndices.size > 1) {
      selectSingle(index);
      await tick();
    }
    await invoke('open', { paths: [results[index].path] }).catch((err) => {
      showToast(`열기 실패: ${String(err)}`);
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
      showToast(`열기 실패: ${String(err)}`);
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
      showToast(`Open With 실패: ${String(err)}`);
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
      showToast(`Finder 표시 실패: ${String(err)}`);
    }
  }

  async function copySelectedPaths() {
    const paths = selectedPaths();
    if (paths.length === 0) {
      return;
    }

    try {
      const payload = await invoke('copy_paths', { paths });
      await navigator.clipboard.writeText(payload);
      showToast(`경로 ${paths.length}개 복사 완료`);
    } catch (err) {
      showToast(`경로 복사 실패: ${String(err)}`);
    }
  }

  async function trashSelected() {
    const paths = selectedPaths();
    if (paths.length === 0) {
      return;
    }

    const message =
      paths.length === 1
        ? '선택한 항목을 휴지통으로 이동할까요?'
        : `${paths.length}개 항목을 휴지통으로 이동할까요?`;

    if (!window.confirm(message)) {
      return;
    }

    try {
      await invoke('move_to_trash', { paths });
      showToast('휴지통으로 이동했습니다.');
      clearSelection();
      await runSearch();
    } catch (err) {
      showToast(`휴지통 이동 실패: ${String(err)}`);
    }
  }

  async function resetIndex() {
    try {
      await invoke('reset_index');
      scanned = 0;
      indexed = 0;
      currentPath = '';
      results = [];
      clearSelection();
      showToast('인덱스를 초기화하고 재구축을 시작했습니다.');
    } catch (err) {
      showToast(`인덱스 초기화 실패: ${String(err)}`);
    }
  }

  function isMultiSelected() {
    return selectedIndices.size > 1;
  }

  async function startRename() {
    if (isMultiSelected()) {
      return;
    }

    const idx = primaryIndex();
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
        new_name: nextName
      });
      cancelRename();
      await runSearch();
    } catch (err) {
      showToast(`이름 변경 실패: ${String(err)}`);
      await tick();
      renameInputEl?.focus();
    }
  }

  function onGlobalClick() {
    closeContextMenu();
  }

  async function focusSearch() {
    await getCurrentWindow().show();
    await getCurrentWindow().setFocus();
    searchInputEl?.focus();
    searchInputEl?.select();
  }

  async function handleKeydown(event) {
    const isMetaSelectAll = (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'a';
    if (isMetaSelectAll) {
      const target = event.target;
      const isTextInput =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target?.isContentEditable;

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
      await startRename();
      return;
    }

    if (event.key === 'F2') {
      event.preventDefault();
      await startRename();
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
      event.preventDefault();
      await trashSelected();
      return;
    }
  }

  function formatKind(entry) {
    if (entry.isDir) {
      return 'Folder';
    }

    if (entry.ext) {
      return entry.ext.toUpperCase();
    }

    return 'File';
  }

  function formatModified(entry) {
    if (!entry.mtime) {
      return '';
    }

    return new Date(entry.mtime * 1000).toLocaleString();
  }

  function formatLastUpdated(timestamp) {
    if (!timestamp) {
      return '-';
    }

    return new Date(timestamp * 1000).toLocaleString();
  }

  let unlistenFns = [];

  onMount(async () => {
    updateViewportHeight();

    await refreshStatus();
    if (!localStorage.getItem('fastfind-fda-notice-v1')) {
      showToast('전체 디스크 검색을 위해 macOS Full Disk Access 권한이 필요할 수 있습니다.');
      localStorage.setItem('fastfind-fda-notice-v1', '1');
    }
    await focusSearch();
    await runSearch();

    const unlistenProgress = await listen('index_progress', (event) => {
      scanned = event.payload.scanned;
      indexed = event.payload.indexed;
      currentPath = event.payload.currentPath;
    });

    const unlistenState = await listen('index_state', (event) => {
      indexStatus = {
        ...indexStatus,
        state: event.payload.state,
        message: event.payload.message ?? null
      };
    });

    const unlistenUpdated = await listen('index_updated', (event) => {
      indexStatus = {
        ...indexStatus,
        entriesCount: event.payload.entriesCount,
        lastUpdated: event.payload.lastUpdated,
        permissionErrors: event.payload.permissionErrors ?? indexStatus.permissionErrors
      };

      if (query.trim().length === 0) {
        void runSearch();
      }
    });

    const unlistenFocus = await listen('focus_search', () => {
      void focusSearch();
    });

    unlistenFns = [unlistenProgress, unlistenState, unlistenUpdated, unlistenFocus];

    window.addEventListener('resize', updateViewportHeight);
    window.addEventListener('click', onGlobalClick);
  });

  onDestroy(async () => {
    clearTimeout(searchTimer);
    clearTimeout(toastTimer);

    for (const unlisten of unlistenFns) {
      unlisten();
    }

    window.removeEventListener('resize', updateViewportHeight);
    window.removeEventListener('click', onGlobalClick);
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
      placeholder="파일/폴더 이름 검색"
      autocomplete="off"
      spellcheck="false"
    />
  </header>

  <section class="table-area">
    <div class="table-header">
      <button type="button" class="col name col-button" on:click={() => handleHeaderSort('name')}>
        Name{sortMark('name')}
      </button>
      <div class="col path">Path</div>
      <div class="col kind">Kind</div>
      <button type="button" class="col modified col-button" on:click={() => handleHeaderSort('mtime')}>
        Modified{sortMark('mtime')}
      </button>
    </div>

    <div
      class="table-body"
      bind:this={tableContainer}
      on:scroll={() => {
        scrollTop = tableContainer.scrollTop;
      }}
    >
      <div class="spacer" style={`height:${totalHeight}px`}>
        <div class="rows" style={`transform: translateY(${translateY}px);`}>
          {#each visibleRows as entry, localIndex}
            {@const index = startIndex + localIndex}
            <div
              class="row {selectedIndices.has(index) ? 'selected' : ''}"
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
                  <span class="ellipsis">{entry.name}</span>
                {/if}
              </div>
              <div class="cell path ellipsis">{entry.dir}</div>
              <div class="cell kind">{formatKind(entry)}</div>
              <div class="cell modified">{formatModified(entry)}</div>
            </div>
          {/each}
        </div>
      </div>
    </div>
  </section>

  <footer class="status-bar">
    <span>Index: {indexStatus.state}</span>
    <span>Entries: {indexStatus.entriesCount.toLocaleString()}</span>
    <span>Last Updated: {formatLastUpdated(indexStatus.lastUpdated)}</span>
    <button class="status-btn" on:click={resetIndex}>Reset Index</button>
    {#if indexStatus.state === 'Indexing'}
      <span class="index-progress">Scanned {scanned.toLocaleString()} / Indexed {indexed.toLocaleString()}</span>
      <span class="path-preview">{currentPath}</span>
    {/if}
    {#if indexStatus.permissionErrors > 0}
      <span class="status-warning">권한 오류: {indexStatus.permissionErrors.toLocaleString()}건</span>
    {/if}
    {#if indexStatus.message}
      <span class="status-error">{indexStatus.message}</span>
    {/if}
  </footer>

  {#if contextMenu.visible}
    <div class="context-menu" style={`left:${contextMenu.x}px;top:${contextMenu.y}px;`}>
      <button on:click={() => (closeContextMenu(), openSelected())}>Open</button>
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
  :global(html, body) {
    margin: 0;
    width: 100%;
    height: 100%;
    overflow: hidden;
    background: #f4f5f7;
    color: #0f1720;
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
  }

  .search-bar {
    padding: 8px;
    background: linear-gradient(180deg, #f7f8fa 0%, #eceff3 100%);
    border-bottom: 1px solid #d8dde4;
  }

  .search-input {
    width: 100%;
    height: 32px;
    border: 1px solid #b9c2cd;
    border-radius: 6px;
    padding: 0 10px;
    font-size: 14px;
    background: #ffffff;
  }

  .table-area {
    display: grid;
    grid-template-rows: auto 1fr;
    min-height: 0;
  }

  .table-header,
  .row {
    display: grid;
    grid-template-columns: minmax(260px, 2fr) minmax(320px, 3fr) 120px 190px;
    align-items: center;
  }

  .table-header {
    height: 30px;
    border-bottom: 1px solid #d8dde4;
    background: #eff2f6;
    user-select: none;
  }

  .col {
    padding: 0 8px;
    font-size: 12px;
    font-weight: 600;
    color: #445060;
    cursor: default;
  }

  .col-button {
    border: none;
    margin: 0;
    background: transparent;
    text-align: left;
    cursor: pointer;
  }

  .table-body {
    overflow: auto;
    min-height: 0;
    background: #ffffff;
  }

  .spacer {
    position: relative;
  }

  .rows {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
  }

  .row {
    height: 28px;
    border-bottom: 1px solid #f0f3f6;
    cursor: default;
  }

  .row:hover {
    background: #eef5ff;
  }

  .row.selected {
    background: #d6e7ff;
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

  .ellipsis {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
    border: 1px solid #8bb0ff;
    border-radius: 4px;
    padding: 0 4px;
    background: #ffffff;
  }

  .status-bar {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 6px 8px;
    background: #eff2f6;
    border-top: 1px solid #d8dde4;
    font-size: 11px;
    color: #4d5a6a;
    white-space: nowrap;
    overflow: hidden;
  }

  .status-btn {
    border: 1px solid #b8c3cf;
    border-radius: 5px;
    background: #ffffff;
    color: #304256;
    font-size: 11px;
    height: 22px;
    padding: 0 8px;
  }

  .path-preview {
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .status-error {
    color: #b64545;
  }

  .status-warning {
    color: #8f6500;
  }

  .context-menu {
    position: fixed;
    z-index: 200;
    width: 220px;
    border: 1px solid #cfd6df;
    border-radius: 8px;
    box-shadow: 0 10px 28px rgba(26, 34, 44, 0.2);
    background: #ffffff;
    overflow: hidden;
  }

  .context-menu button {
    width: 100%;
    text-align: left;
    border: none;
    background: transparent;
    padding: 8px 10px;
    font-size: 12px;
    color: #213042;
  }

  .context-menu button:hover {
    background: #edf3ff;
  }

  .toast {
    position: fixed;
    right: 14px;
    bottom: 18px;
    background: rgba(29, 37, 49, 0.95);
    color: #ffffff;
    border-radius: 8px;
    padding: 9px 12px;
    font-size: 12px;
    z-index: 250;
  }
</style>
