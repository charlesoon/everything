# Architecture

macOS "Everything" — 홈 디렉토리 전체를 SQLite에 인덱싱하여 sub-50ms 파일/폴더 이름 검색을 제공하는 Tauri v2 데스크톱 앱.

## Tech Stack

| Layer | Technology |
|-------|-----------|
| App framework | Tauri v2 |
| Backend | Rust (rusqlite, jwalk, fsevent-sys, ignore) |
| Frontend | Svelte 4 (단일 컴포넌트) |
| DB | SQLite WAL mode, LIKE 기반 검색 |
| Build | Vite 5, Cargo |

---

## Module Structure

```
src-tauri/src/
├── main.rs              # 앱 상태, DB, 인덱싱, 검색, 파일 액션, IPC 핸들러
├── query.rs             # 검색 쿼리 파서 (SearchMode 분류)
├── fd_search.rs         # jwalk 기반 라이브 파일시스템 검색
├── fsevent_watcher.rs   # FSEvents 직접 바인딩 (macOS only)
├── gitignore_filter.rs  # .gitignore 재귀 탐색 및 매칭
└── spotlight_search.rs  # mdfind 기반 Spotlight 검색 fallback

src/
├── main.js              # Svelte 마운트 포인트
└── App.svelte           # 전체 UI (1,755줄 단일 컴포넌트)
```

### 모듈 의존성

```
main.rs ──→ query.rs            (쿼리 파싱)
        ──→ fd_search.rs        (라이브 검색)
        ──→ fsevent_watcher.rs  (파일 감시, macOS only)
        ──→ gitignore_filter.rs (.gitignore 필터)
        ──→ spotlight_search.rs (Spotlight fallback)

query.rs           독립 (의존성 없음)
fd_search.rs    ──→ main.rs (EntryDto, should_skip_path, IgnorePattern)
spotlight_search.rs ──→ main.rs (EntryDto)
gitignore_filter.rs    독립 (ignore 크레이트만 사용)
fsevent_watcher.rs     독립 (fsevent-sys만 사용)
```

---

## App State

```rust
struct AppState {
    db_path: PathBuf,                     // index.db 경로
    home_dir: PathBuf,                    // $HOME
    cwd: PathBuf,                         // 현재 작업 디렉토리
    db_ready: Arc<AtomicBool>,            // DB 초기화 완료 여부
    indexing_active: Arc<AtomicBool>,     // 인덱싱 진행 중 플래그
    status: Arc<Mutex<IndexStatus>>,      // 인덱싱 상태 (state, counts)
    path_ignores: Arc<Vec<PathBuf>>,      // 무시 경로 목록
    path_ignore_patterns: Arc<Vec<IgnorePattern>>,  // 무시 패턴 (glob)
    gitignore: SharedGitignoreFilter,     // Arc<GitignoreFilter>
    recent_ops: Arc<Mutex<Vec<RecentOp>>>,          // rename/trash 2초 TTL 캐시
    icon_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,   // 확장자→PNG 아이콘
    fd_search_cache: Arc<Mutex<Option<FdSearchCache>>>, // 라이브 검색 캐시
    negative_name_cache: Arc<Mutex<Vec<NegativeNameEntry>>>, // 0건 검색어 60초 캐시
    ignore_cache: Arc<Mutex<Option<IgnoreRulesCache>>>,      // 무시 규칙 mtime 캐시
}
```

모든 필드가 `Arc`로 래핑되어 `Clone` 가능. Tauri `State<AppState>`로 IPC 핸들러에 주입.

---

## DB Schema

**위치**: `<app_data_dir>/index.db` | **버전**: `PRAGMA user_version = 4`

### entries 테이블

```sql
CREATE TABLE entries (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    dir TEXT NOT NULL,
    is_dir INTEGER NOT NULL,
    ext TEXT,
    mtime INTEGER,
    size INTEGER,
    indexed_at INTEGER NOT NULL,
    run_id INTEGER NOT NULL DEFAULT 0
);
```

### 인덱스

| 인덱스 | 용도 |
|--------|------|
| `idx_entries_name_nocase` | `name COLLATE NOCASE` — NameSearch prefix/contains |
| `idx_entries_dir` | `dir` — PathSearch 디렉토리 범위 |
| `idx_entries_dir_ext_name_nocase` | `(dir, ext, name)` — PathSearch + ext shortcut |
| `idx_entries_ext` | `ext` — ExtSearch |
| `idx_entries_ext_name` | `(ext, name)` — ExtSearch + 정렬 |
| `idx_entries_mtime` | `mtime` — 수정일 정렬 |
| `idx_entries_run_id` | `run_id` — 증분 인덱싱 stale row 삭제 |

### meta 테이블

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

| key | 용도 |
|-----|------|
| `last_run_id` | 마지막 인덱싱 run ID (증분 비교 기준) |
| `last_event_id` | FSEvents event ID (재시작 시 replay 시작점) |

### Pragma 설정

```
journal_mode=WAL  synchronous=NORMAL  temp_store=MEMORY  busy_timeout=3000
인덱싱 중: cache_size=64MB  mmap_size=256MB  wal_autocheckpoint=OFF
인덱싱 후: cache_size=16MB  mmap_size=0     wal_autocheckpoint=1000 → TRUNCATE
```

---

## Startup Sequence

```
앱 시작
  │
  ├─ 경로 확인: app_data_dir, db_path, home_dir, cwd
  ├─ 무시 규칙 로드: .pathignore + .gitignore roots + TCC roots
  ├─ GitignoreFilter 빌드: $HOME 하위 비숨김 디렉토리 depth 3까지 탐색
  ├─ AppState 구성 및 등록
  ├─ 글로벌 단축키 등록: Cmd+Shift+F → 창 표시 + focus_search 이벤트
  │
  └─ 백그라운드 스레드 시작
       │
       ├─ init_db(): 테이블/인덱스 생성, 버전 체크 (불일치 시 전체 재생성)
       ├─ purge_ignored_entries(): 무시 대상 기존 DB 엔트리 삭제
       ├─ db_ready = true (검색 가능)
       ├─ emit_status_counts → 프론트엔드에 현재 엔트리 수 전달
       │
       ├─ [조건부 시작] last_event_id 존재 AND DB에 엔트리 있음?
       │    ├─ YES → FSEvents watcher (replay 모드) 시작
       │    │         replay 성공 → Ready (full scan 생략)
       │    │         MustScanSubDirs ≥ 10 → full scan fallback
       │    │
       │    └─ NO  → 증분 인덱싱 시작 + FSEvents watcher (since now) 시작
       │
       └─ 아이콘 프리워밍: 20개 주요 확장자 미리 로드
```

---

## Indexing Flow

### 증분 인덱싱 (`run_incremental_index`)

```
run_incremental_index
  │
  ├─ 인덱싱 pragma 설정 (대용량 캐시, mmap)
  ├─ current_run_id = last_run_id + 1
  ├─ 기존 entries HashMap 프리로드: path → (mtime, size)
  │
  ├─ $HOME 자식 디렉토리 분류
  │    ├─ priority roots: Library/.Trash 제외한 일반 디렉토리
  │    └─ deferred roots: Library, .Trash, .Trashes
  │
  ├─ Pass 0 (shallow): jwalk depth ≤ 6, priority → deferred 순
  │    ├─ mtime+size 변경 없음 → UPDATE run_id만 (경량)
  │    ├─ 변경 또는 신규 → INSERT/UPDATE (전체 컬럼)
  │    ├─ 10,000건마다 batch commit
  │    └─ 200ms마다 index_progress 이벤트 emit
  │    └─ Pass 0 완료 → index_updated 이벤트 (조기 검색 가능)
  │
  ├─ Pass 1 (deep): jwalk 무제한 depth, depth > 6만 처리
  │    └─ (같은 증분 로직)
  │
  ├─ Cleanup: DELETE FROM entries WHERE run_id < current_run_id
  ├─ meta.last_run_id = current_run_id
  ├─ ANALYZE + pragma 복원 + WAL checkpoint
  └─ index_state=Ready, index_updated 이벤트 emit
```

### 무시 체계

```
should_skip_path(path)
  │
  ├─ BUILTIN_SKIP_NAMES: .git, node_modules, .Trash, .npm, .cache,
  │                       CMakeFiles, .qtc_clangd, __pycache__, .gradle
  ├─ BUILTIN_SKIP_PATHS: Library/Caches, Library/Developer/CoreSimulator,
  │                       Library/Logs, .vscode/extensions
  ├─ .pathignore: 프로젝트 루트 및 $HOME에서 로드
  ├─ macOS TCC roots: ~/Library/Mail, Safari, Messages 등 ~40개
  ├─ IgnorePattern::AnySegment: **/target 등 어느 depth에서든 매칭
  ├─ IgnorePattern::Glob: 와일드카드 패턴 매칭
  └─ gitignore_filter: .gitignore 규칙 (ignore 크레이트)
```

---

## Search Flow

### 쿼리 분류 (`query.rs`)

| 입력 패턴 | SearchMode | 예시 |
|-----------|-----------|------|
| 빈 문자열 | `Empty` | `""` |
| `*` 또는 `?` 포함 | `GlobName` | `*.rs`, `test?` |
| `*.ext` (단순 확장자) | `ExtSearch` | `*.pdf` |
| `/` 포함 | `PathSearch` | `src/ main`, `Projects/ *.rs` |
| 그 외 | `NameSearch` | `readme`, `config` |

### 검색 실행 시퀀스 (`execute_search`)

```
사용자 입력
  │
  ├─ DB 미준비 → Spotlight fallback (mdfind) → 반환
  │
  ├─ 쿼리 파싱 → SearchMode 결정
  │
  ├─ [NameSearch] negative cache 확인
  │    ├─ 캐시 hit (300-550ms 이내, 미확인) → find 명령 1회 fallback
  │    └─ 캐시 hit (그 외) → 빈 결과 즉시 반환
  │
  ├─ DB 검색 (모드별)
  │    │
  │    ├─ Empty: SELECT ... ORDER BY sort LIMIT offset
  │    │
  │    ├─ NameSearch (3-phase):
  │    │    Phase 0: name = query (정확 매칭)
  │    │    Phase 1: name LIKE 'query%' (접두사, idx_entries_name_nocase)
  │    │    Phase 2: 8ms probe → 30ms fetch (name LIKE '%query%')
  │    │
  │    ├─ GlobName: name LIKE (glob→LIKE 변환)
  │    │
  │    ├─ ExtSearch: ext = 'ext' (인덱스 직접 조회)
  │    │
  │    └─ PathSearch:
  │         dir 힌트 해석 가능 → dir 범위 쿼리 + ext shortcut
  │         dir 힌트 해석 불가 → dir LIKE + 2-phase probe
  │
  ├─ 결과 0건 + 인덱싱 아님 + GlobName/ExtSearch
  │    → find 명령 fallback (maxdepth 8)
  │
  ├─ 결과 0건 + 인덱싱 중
  │    → Spotlight fallback (mdfind, 3초 타임아웃, 최대 300건)
  │
  ├─ 후처리
  │    ├─ 무시 규칙 필터링
  │    ├─ 관련성 정렬 (name sort, offset=0일 때)
  │    │    rank 0: 정확 매칭
  │    │    rank 1: 접두사 매칭
  │    │    rank 2: 이름 포함
  │    │    rank 3: 경로 끝 매칭
  │    │    rank 4: 경로 포함
  │    │    동일 rank 내 얕은 경로 우선
  │    └─ NameSearch 0건 → negative cache 저장 (60초 TTL)
  │
  └─ SearchResultDto { entries, modeLabel } 반환
```

### Spotlight Fallback (`spotlight_search.rs`)

```
search_spotlight(home_dir, query)
  │
  ├─ query < 2자 → 빈 결과
  ├─ mdfind -name <query> -onlyin <home_dir> 실행
  ├─ stdout 스트리밍 읽기
  │    ├─ 3초 타임아웃 → timed_out = true, 중단
  │    └─ 300건 도달 → 중단
  ├─ child process kill
  └─ SpotlightResult { entries, timed_out }
```

---

## Watcher Flow

### FSEvents 아키텍처 (`fsevent_watcher.rs`)

```
FsEventWatcher::new(root, since_event_id, tx)
  │
  ├─ fsevent_sys 직접 바인딩 (notify 크레이트 미사용)
  ├─ Flags: FileEvents | NoDefer
  ├─ Latency: 0.3초
  ├─ 전용 스레드 "everything-fsevents"에서 CFRunLoop 실행
  │
  └─ 콜백 → FsEvent 분류
       ├─ HistoryDone      (replay 완료)
       ├─ MustScanSubDirs  (subtree 재스캔 필요)
       └─ Paths            (일반 파일 변경)
```

### Watcher 이벤트 처리 (`start_fsevent_watcher_worker`)

```
이벤트 수신 루프 (100ms recv_timeout)
  │
  ├─ Paths → pending_paths에 추가, 디바운스 타이머 설정 (300ms)
  │
  ├─ MustScanSubDirs → 즉시 subtree 재스캔 + upsert
  │    (conditional startup 중 count ≥ 10 → full scan 트리거)
  │
  ├─ HistoryDone → pending 즉시 flush
  │    (conditional startup 종료)
  │
  ├─ 디바운스 만료 → process_watcher_paths()
  │    ├─ indexing_active 중이면 스킵
  │    ├─ 각 경로: should_skip / is_recently_touched 체크
  │    ├─ 존재하는 경로 → upsert (디렉토리면 자식도)
  │    └─ 없는 경로 → DB에서 삭제
  │
  └─ 30초마다 last_event_id를 meta 테이블에 flush
```

---

## IPC Commands

| Command | 방향 | 설명 |
|---------|------|------|
| `get_index_status` | FE→BE | 인덱싱 상태, 엔트리 수, 진행률 |
| `get_home_dir` | FE→BE | 홈 디렉토리 경로 |
| `start_full_index` | FE→BE | 전체 재인덱싱 트리거 |
| `reset_index` | FE→BE | DB 초기화 후 재인덱싱 |
| `search` | FE→BE | DB 검색 → `SearchResultDto { entries, modeLabel }` |
| `fd_search` | FE→BE | jwalk 라이브 검색 → `FdSearchResultDto { entries, total, timedOut }` |
| `open` | FE→BE | `open` 명령으로 열기 (디렉토리 실패 시 `open -R` fallback) |
| `open_with` | FE→BE | Finder에서 보기 |
| `reveal_in_finder` | FE→BE | `open -R` (단일) / 부모 디렉토리 열기 (다중) |
| `copy_paths` | FE→BE | 경로 클립보드 복사 (pbcopy) |
| `move_to_trash` | FE→BE | 휴지통 이동 + DB 삭제 |
| `rename` | FE→BE | 이름 변경 + DB 갱신 → 새 EntryDto 반환 |
| `get_file_icon` | FE→BE | 확장자별 시스템 아이콘 PNG 반환 |

## Backend Events

| Event | Payload | 시점 |
|-------|---------|------|
| `index_progress` | `{ scanned, indexed, currentPath }` | 인덱싱 중 200ms 간격 |
| `index_state` | `{ state, message }` | Indexing/Ready/Error 전환 시 |
| `index_updated` | `{ entriesCount, lastUpdated, permissionErrors }` | 인덱싱 완료, watcher 업데이트, 파일 액션 후 |
| `focus_search` | (없음) | Cmd+Shift+F 글로벌 단축키 |

---

## Frontend Architecture

### 단일 컴포넌트 (`App.svelte`)

검색 입력, 가상 스크롤 테이블, 인라인 이름 변경, 컨텍스트 메뉴, 키보드 단축키, 아이콘 캐시, 상태 바를 모두 포함하는 1,755줄 단일 컴포넌트.

### 상태 관리

Svelte 리액티브 변수 사용 (store 미사용).

| 카테고리 | 주요 변수 |
|---------|----------|
| 검색 | `query`, `results`, `searchGeneration`, `dbLatencyMs`, `searchModeLabel`, `sortBy`, `sortDir` |
| 선택 | `selectedIndices` (Set), `selectionAnchor`, `lastSelectedIndex` |
| 편집 | `editing { active, path, index, draftName }` |
| 인덱싱 | `indexStatus`, `scanned`, `indexed`, `currentPath`, `lastReadyCount` |
| 가상 스크롤 | `scrollTop`, `viewportHeight`, `colWidths` |
| 캐시 | `iconCache` (Map), `highlightCache` (Map) |

### 검색 입력 → 결과 시퀀스

```
사용자 타이핑
  │
  ├─ on:input → scheduleSearch()
  │    ├─ 200ms 이상 경과 → 즉시 실행 (leading edge)
  │    └─ 200ms 미만 → 200ms 후 실행 (trailing edge)
  │
  ├─ runSearch()
  │    ├─ searchGeneration++ (stale 응답 방지)
  │    ├─ invoke('search', { query, limit: 500, offset: 0, sort_by, sort_dir })
  │    ├─ 응답: { entries, modeLabel }
  │    ├─ results = entries
  │    ├─ searchModeLabel = modeLabel
  │    └─ 선택 경로 기반 복원
  │
  └─ 무한 스크롤
       스크롤 하단 10행 이내 → loadMore()
       → invoke('search', { offset: results.length })
       → results에 append
```

### 가상 스크롤

```
고정 행 높이: 28px
버퍼: 상하 10행 (총 overscan ~20행)

scrollTop
  → startIndex = max(0, floor(scrollTop / 28) - 10)
  → endIndex = min(results.length, startIndex + visibleCount)
  → visibleRows = results.slice(startIndex, endIndex)
  → translateY = startIndex * 28

DOM:
  <div class="table-body">          ← 스크롤 컨테이너
    <div style="height:{totalHeight}px">  ← 전체 가상 높이
      <div style="transform:translateY({translateY}px)">  ← 오프셋
        {#each visibleRows}...
      </div>
    </div>
  </div>
```

### 키보드 단축키

| 키 | 동작 |
|----|------|
| `Escape` | 선택 해제, 검색 입력 포커스 |
| `↑` / `↓` | 선택 이동 (Shift: 범위 선택) |
| `PageUp` / `PageDown` | 페이지 단위 이동 |
| `Enter` | 인라인 이름 변경 시작 |
| `F2` | 인라인 이름 변경 시작 |
| `Cmd+O` | 선택 항목 열기 |
| `Cmd+Enter` | Finder에서 보기 |
| `Cmd+C` | 경로 복사 |
| `Cmd+A` | 전체 선택 |
| `Delete` / `Cmd+⌫` | 휴지통 이동 |

### 컨텍스트 메뉴

우클릭 시 표시: Open, Open With, Reveal in Finder, Copy Path, Move to Trash, Rename (단일 선택 시)

### 아이콘 시스템

```
visibleRows 변경
  → ensureIcon(entry) 호출
  → iconCache에 있으면 즉시 반환
  → 없으면 invoke('get_file_icon', { ext })
       → 백엔드: swift -e NSWorkspace 16x16 PNG
       → base64 data URI로 변환 후 iconCache에 저장
  → 시작 시 20개 주요 확장자 프리워밍
```

### 테마

시스템 설정 연동 (`prefers-color-scheme: dark`). CSS 커스텀 프로퍼티 기반.

```css
:root {
  --bg-app, --text-primary, --surface, --row-hover,
  --row-selected, --border-soft, --focus-ring, ...
}
```

### 상태 바

| 상태 | 표시 내용 |
|------|----------|
| 인덱싱 중 (엔트리 있음) | `● 검색 가능` + 진행률% + 경과 시간 + 엔트리 수 |
| 인덱싱 중 (엔트리 없음) | `인덱싱 시작 중...` |
| Ready | `Index: Ready` + 엔트리 수 + 인덱싱 소요 시간 |
| Spotlight fallback | 주황색 `Spotlight 임시 검색` |
| 검색 완료 | `"query" Xms · N results` |

---

## Key Constants

| 상수 | 값 | 위치 |
|------|---|------|
| `DEFAULT_LIMIT` | 300 | 검색 기본 결과 수 |
| `SHORT_QUERY_LIMIT` | 100 | 1자 쿼리 결과 제한 |
| `MAX_LIMIT` | 1,000 | 최대 결과 수 |
| `BATCH_SIZE` | 10,000 | DB 배치 쓰기 단위 |
| `SHALLOW_SCAN_DEPTH` | 6 | Pass 0 최대 depth |
| `JWALK_NUM_THREADS` | 4 | 병렬 워커 수 |
| `WATCH_DEBOUNCE` | 300ms | 파일 변경 디바운스 |
| `RECENT_OP_TTL` | 2s | rename/trash 중복 방지 |
| `NEGATIVE_CACHE_TTL` | 60s | 0건 검색어 캐시 |
| `SPOTLIGHT_TIMEOUT` | 3s | mdfind 타임아웃 |
| `SPOTLIGHT_MAX_RESULTS` | 300 | mdfind 최대 결과 |
| `MUST_SCAN_THRESHOLD` | 10 | replay 중 full scan 트리거 |
| `EVENT_ID_FLUSH_INTERVAL` | 30s | event_id DB 저장 주기 |
| `PAGE_SIZE` (FE) | 500 | 프론트엔드 페이지 크기 |
| `rowHeight` (FE) | 28px | 가상 스크롤 행 높이 |
