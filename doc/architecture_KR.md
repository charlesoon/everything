# Architecture

Everything — 파일 시스템 전체를 SQLite에 인덱싱하여 sub-50ms 파일/폴더 이름 검색을 제공하는 GPUI 데스크톱 앱. macOS와 Windows를 지원.

## Tech Stack

| Layer | Technology |
|-------|-----------|
| App framework | GPUI |
| Backend | Rust (rusqlite, jwalk, ignore) |
| Frontend | Rust + gpui-component |
| DB | SQLite WAL mode, LIKE 기반 검색 |
| Build | Cargo |
| macOS watcher | fsevent-sys (FSEvents 직접 바인딩) |
| Windows 인덱서 | MFT 스캔 (Win32 FSCTL), USN 저널, ReadDirectoryChangesW fallback, WalkDir non-admin fallback |
| Windows 추가 | windows 0.58, notify, rayon, png |
| Native integrations | global-hotkey, drag, GPUI native window lifecycle |

---

## Module Structure

```
src-tauri/src/
├── ui/mod.rs, view.rs, actions.rs  # Native GPUI lifecycle, views, file actions
├── ui_model.rs, ui_prefs.rs      # Generations, selection, pagination, preferences
├── runtime.rs, ui_gate.rs        # Worker executor, coalesced status event bridge
├── main.rs                      # Shared app setup, search, DB and file APIs
├── query.rs, mem_search.rs       # Query parser and in-memory search
├── daemon.rs, daemon_ipc.rs      # Daemon lifecycle and control socket
├── mcp_server.rs                # Read-only MCP stdio server
├── mac/                         # FSEvents, daemon bridge, login item, FDA, icons
└── win/                         # MFT/USN/RDCW, service, Shell menu and icons
scripts/bundle_macos.py           # Native .app/.dmg packaging
src-tauri/windows/installer.nsi   # Native NSIS installer and service hooks
```

### 모듈 의존성

```
main.rs ──→ query.rs            (쿼리 파싱)
        ──→ fd_search.rs        (라이브 검색)
        ──→ mem_search.rs       (인메모리 검색)
        ──→ gitignore_filter.rs (.gitignore 필터)
        ──→ mac::*              (macOS: FSEvents, Spotlight)
        ──→ win::*              (Windows: MFT, USN, RDCW, 아이콘, 컨텍스트 메뉴)

query.rs              독립 (의존성 없음)
fd_search.rs       ──→ main.rs (EntryDto, should_skip_path, IgnorePattern)
mem_search.rs      ──→ main.rs (EntryDto), query.rs (SearchMode)
gitignore_filter.rs   독립 (ignore 크레이트만 사용)

mac/fsevent_watcher.rs    독립 (fsevent-sys만 사용)
mac/spotlight_search.rs ──→ main.rs (EntryDto)

win/mod.rs         ──→ mft_indexer, nonadmin_indexer, usn_watcher, rdcw_watcher, search_catchup
win/mft_indexer.rs ──→ path_resolver, volume, com_guard, mem_search
win/nonadmin_indexer.rs ──→ mem_search, rdcw_watcher
win/usn_watcher.rs ──→ path_resolver, volume, com_guard
win/rdcw_watcher.rs   독립 (notify 크레이트만 사용)
win/icon.rs        ──→ com_guard
win/context_menu.rs ──→ com_guard
win/volume.rs         독립 (windows 크레이트만 사용)
win/path_resolver.rs  독립
win/com_guard.rs      독립 (windows 크레이트만 사용)
```

---

## App State

```rust
struct AppState {
    db_path: PathBuf,                     // index.db 경로
    home_dir: PathBuf,                    // $HOME (macOS) / %USERPROFILE% (Windows)
    scan_root: PathBuf,                   // $HOME (macOS) / C:\ (Windows)
    cwd: PathBuf,                         // 현재 작업 디렉토리
    db_ready: Arc<AtomicBool>,            // DB 초기화 완료 여부
    indexing_active: Arc<AtomicBool>,     // 인덱싱 진행 중 플래그
    status: Arc<Mutex<IndexStatus>>,      // 인덱싱 상태 (state, counts, backgroundActive)
    path_ignores: Arc<Vec<PathBuf>>,      // 무시 경로 목록
    path_ignore_patterns: Arc<Vec<IgnorePattern>>,  // 무시 패턴 (glob)
    gitignore: Arc<LazyGitignoreFilter>,  // 지연 .gitignore 필터
    recent_ops: Arc<Mutex<Vec<RecentOp>>>,          // rename/trash 2초 TTL 캐시
    icon_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,   // 확장자→PNG 아이콘
    fd_search_cache: Arc<Mutex<Option<FdSearchCache>>>, // 라이브 검색 캐시
    negative_name_cache: Arc<Mutex<HashMap<String, NegativeNameEntry>>>, // 0건 검색어 60초 캐시
    ignore_cache: Arc<Mutex<Option<IgnoreRulesCache>>>,      // 무시 규칙 mtime 캐시
    mem_index: Arc<RwLock<Option<Arc<MemIndex>>>>,  // 인메모리 인덱스 (Windows: MFT→DB upsert 중)
    watcher_stop: Arc<AtomicBool>,        // 파일 워처 중지 신호
    watcher_active: Arc<AtomicBool>,      // 파일 워처 이벤트 루프 실행 중
    frontend_ready: Arc<AtomicBool>,      // 프론트엔드 onMount 완료
}
```

모든 필드가 `Arc`로 래핑되어 `Clone` 가능. 네이티브 Rust API에 직접 전달한다.

---

## DB Schema

**위치**: `<app_data_dir>/index.db` | **버전**: `PRAGMA user_version = 5`

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
| `idx_entries_dir_ext_name_nocase` | `(dir, ext, name)` — PathSearch + ext shortcut |
| `idx_entries_ext_name` | `(ext, name)` — ExtSearch + 정렬 |
| `idx_entries_mtime` | `mtime` — 수정일 정렬 |
| `idx_entries_indexed_at` | `indexed_at` — stale row 관리 |

### meta 테이블

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

| key | 용도 |
|-----|------|
| `last_run_id` | 마지막 인덱싱 run ID (증분 비교 기준) |
| `last_event_id` | FSEvents event ID — 재시작 시 replay 시작점 (macOS) |
| `win_last_usn` | 다음 USN 오프셋 — 저널 이어읽기 (Windows) |
| `win_journal_id` | USN 저널 ID — 저널 리셋 감지 (Windows) |
| `index_complete` | 이전 인덱싱 정상 완료 플래그 (Windows) |
| `rdcw_last_active_ts` | RDCW 오프라인 catchup용 마지막 활성 타임스탬프 (Windows) |

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
  ├─ 경로 확인: app_data_dir, db_path, home_dir, scan_root, cwd
  │    macOS:   home_dir = $HOME,          scan_root = $HOME
  │    Windows: home_dir = %USERPROFILE%,  scan_root = C:\
  │
  ├─ 무시 규칙 로드: .pathignore + .gitignore roots
  │    macOS:   + TCC roots (~40개 보호된 Library 경로)
  │
  ├─ LazyGitignoreFilter 빌드: scan_root 하위 비숨김 디렉토리 depth 3까지 탐색
  ├─ AppState 구성 및 등록
  │
  ├─ 글로벌 단축키 등록 (macOS만):
  │    Cmd+Shift+Space → 창 표시 + focus_search 이벤트
  │
  ├─ 윈도우 설정:
  │    macOS:   vibrancy 적용 (NSVisualEffectMaterial::UnderWindowBackground)
  │    Windows: 시스템 테마에 따른 배경색 설정 (흰색 깜빡임 방지)
  │
  └─ 백그라운드 스레드 시작
       │
       ├─ init_db(): 테이블 생성, 버전 체크 (불일치 시 전체 재생성)
       ├─ purge_ignored_entries(): 무시 대상 기존 DB 엔트리 삭제
       ├─ db_ready = true (검색 가능)
       ├─ ensure_db_indexes(): 인덱스 생성 (비차단, 지연)
       ├─ emit_status_counts → 프론트엔드에 현재 엔트리 수 전달
       │
       ├─ [macOS] 조건부 시작: last_event_id 존재 AND DB에 엔트리 있음?
       │    ├─ YES → FSEvents watcher (replay 모드) 시작
       │    │         replay 성공 → Ready (full scan 생략)
       │    │         MustScanSubDirs ≥ 10 → full scan fallback
       │    └─ NO  → 증분 인덱싱 시작 + FSEvents watcher (since now) 시작
       │
       ├─ [Windows] start_windows_indexing():
       │    ├─ 저장된 USN, journal ID, index_complete 플래그 읽기
       │    ├─ 이전 인덱스 완료 상태 → Ready 즉시 설정 (catchup 중 검색 가능)
       │    └─ 워커 스레드 생성 (아래 Windows 인덱싱 흐름 참조)
       │
       └─ 아이콘 프리워밍 (macOS만): 20개 주요 확장자 미리 로드
```

---

## Indexing Flow

### macOS: 증분 인덱싱 (`run_incremental_index`)

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

### Windows: MFT 인덱싱 (`win::mft_indexer`)

```
MFT 스캔 (NTFS Master File Table)
  │
  ├─ open_volume('C') → NTFS 볼륨 핸들 획득
  ├─ query_usn_journal() → journal_id, next_usn
  │
  ├─ Pass 1: MFT 레코드 열거
  │    ├─ FSCTL_ENUM_USN_DATA로 전체 MFT 순회
  │    ├─ PathResolver 빌드: 디렉토리 FRN → (parent_frn, name) 맵
  │    └─ 파일 레코드 수집: FRN, parent_frn, name, attributes
  │
  ├─ Pass 2: 경로 해석 + upsert (rayon 병렬)
  │    ├─ 각 파일 FRN → PathResolver로 전체 경로 해석
  │    ├─ 필터: scan_root 외부 경로 스킵, 무시 규칙 적용
  │    ├─ MemIndex 빌드 (DB upsert 중 즉시 검색용)
  │    ├─ 백그라운드 DB upsert 파이프라인 (배치 50,000건)
  │    └─ 200ms마다 index_progress emit
  │
  ├─ stale 엔트리 정리 + ANALYZE
  ├─ win_last_usn, win_journal_id, index_complete을 meta에 저장
  └─ FRN 캐시 + next_usn을 USN watcher에 전달
```

### Windows: Non-Admin 인덱서 (`win::nonadmin_indexer`)

```
WalkDir fallback (MFT 접근 불가 시)
  │
  ├─ Phase 1 (shallow): 우선순위 루트 depth 제한 스캔
  │    ├─ 조기 MemIndex 빌드 (즉시 검색용)
  │    └─ 스캔 중 index_progress emit
  │
  ├─ Phase 2 (deep): 나머지 루트 병렬 스캔
  │    ├─ rayon으로 루트별 병렬 스캔
  │    └─ 전체 MemIndex 빌드
  │
  ├─ 백그라운드 DB 저장 (벌크 인서트)
  │    ├─ 인덱스 삭제/재생성으로 빠른 upsert
  │    ├─ stale 엔트리 정리 + ANALYZE
  │    └─ DB 완료 후 MemIndex 해제
  │
  └─ RDCW watcher 시작 (증분 업데이트용)
```

### Windows 인덱싱 fallback 체인

```
start_windows_indexing()
  │
  ├─ MFT 스캔 시도 (가장 빠름, 볼륨 접근 필요)
  │    ├─ 성공 → FRN 캐시와 함께 USN watcher 시작
  │    └─ 실패 ↓
  │
  ├─ DB에 이전 데이터 있으면 → search_catchup (오프라인 동기화)
  │    ├─ Windows Search 서비스 시도 (ADODB via PowerShell, 10초 타임아웃)
  │    └─ Fallback: mtime 기반 WalkDir 스캔
  │
  ├─ USN watcher만 시도 (MFT 캐시 없이)
  │    └─ 실패 ↓
  │
  ├─ Non-admin WalkDir 인덱서 시도 (nonadmin_indexer)
  │    └─ 실패 ↓
  │
  └─ RDCW watcher fallback (ReadDirectoryChangesW via notify 크레이트)
```

### 무시 체계

```
should_skip_path(path)
  │
  ├─ BUILTIN_SKIP_NAMES: .git, node_modules, .Trash, .Trashes, .npm, .cache,
  │                       CMakeFiles, .qtc_clangd, __pycache__, .gradle, DerivedData
  │
  ├─ BUILTIN_SKIP_SUFFIXES: .build (Xcode 중간 빌드 디렉토리)
  │
  ├─ BUILTIN_SKIP_PATHS (macOS):
  │    Library/Caches, Library/Developer/CoreSimulator, Library/Logs
  │
  ├─ BUILTIN_SKIP_PATHS (크로스 플랫폼):
  │    .vscode/extensions
  │
  ├─ BUILTIN_SKIP_PATHS (Windows — AppData 노이즈):
  │    AppData/Local/Temp, AppData/Local/Microsoft,
  │    AppData/Local/Google, AppData/Local/Packages
  │
  ├─ DEFERRED_DIR_NAMES (Windows — 시스템 디렉토리):
  │    Windows, Program Files, Program Files (x86), $Recycle.Bin,
  │    System Volume Information, Recovery, PerfLogs
  │
  ├─ .pathignore: 프로젝트 루트 및 home_dir에서 로드
  ├─ macOS TCC roots: ~/Library/Mail, Safari, Messages 등 ~40개
  ├─ IgnorePattern::AnySegment: **/target 등 어느 depth에서든 매칭
  ├─ IgnorePattern::Glob: 와일드카드 패턴 매칭
  └─ LazyGitignoreFilter: .gitignore 규칙 (ignore 크레이트, depth 3)
```

---

## Search Flow

### 쿼리 분류 (`query.rs`)

| 입력 패턴 | SearchMode | 예시 |
|-----------|-----------|------|
| 빈 문자열 | `Empty` | `""` |
| `*` 또는 `?` 포함 | `GlobName` | `*.rs`, `test?` |
| `*.ext` (단순 확장자) | `ExtSearch` | `*.pdf` |
| `/` 또는 `\` 포함 | `PathSearch` | `src/ main`, `Projects/ *.rs` |
| 그 외 | `NameSearch` | `readme`, `config` |

### 검색 실행 시퀀스 (`execute_search`)

```
사용자 입력
  │
  ├─ DB 미준비 → Spotlight fallback (macOS만, mdfind) → 반환
  │
  ├─ 쿼리 파싱 → SearchMode 결정
  │
  ├─ [NameSearch] negative cache 확인
  │    ├─ 캐시 hit (300-550ms 이내, 미확인) → find 명령 1회 fallback
  │    └─ 캐시 hit (그 외) → 빈 결과 즉시 반환
  │
  ├─ MemIndex 확인 (Windows, MFT→DB upsert 중)
  │    └─ MemIndex 존재 → 인메모리 검색, 결과 반환
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
  ├─ 결과 0건 + 인덱싱 중 (macOS)
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
  └─ SearchResultDto { entries, modeLabel, totalCount, totalKnown } 반환
```

### 인메모리 검색 (`mem_search.rs`)

```
MemIndex (MFT/WalkDir 스캔 중 즉시 검색용으로 빌드)
  │
  ├─ CompactEntry: 최소 구조체 (EntryDto 대비 ~104바이트 절감)
  ├─ sorted_idx: 이름 정렬 인덱스 (이진 검색용)
  ├─ ext_map: 확장자 → 엔트리 인덱스
  ├─ dir_map: 디렉토리 → 엔트리 인덱스
  │
  ├─ 검색 단계:
  │    Phase 1: 정확 + 접두사 (이진 검색)
  │    Phase 2: 포함 매칭 (30ms 시간 제한)
  │
  └─ 정렬: 관련성 (0-9) → mtime/size/name
```

### Spotlight Fallback (macOS 전용 — `mac/spotlight_search.rs`)

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

### macOS: FSEvents 아키텍처 (`mac/fsevent_watcher.rs`)

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

### macOS: Watcher 이벤트 처리 (`start_fsevent_watcher_worker`)

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

### Windows: USN Journal Watcher (`win/usn_watcher.rs`)

```
USN watcher (주요 — MFT 스캔 이후)
  │
  ├─ MFT 인덱서로부터 FRN→경로 캐시 수신 (syscall 없이 경로 해석)
  ├─ 저장된 next_usn부터 FSCTL_READ_USN_JOURNAL 폴링
  ├─ USN reason 필터: CREATE, DELETE, RENAME_OLD/NEW, CLOSE
  │    (메타데이터 변경만은 스킵)
  │
  ├─ rename 페어링: RENAME_OLD_NAME + RENAME_NEW_NAME을 500ms 타임아웃으로 매칭
  │    불완전 페어 → create 또는 delete로 처리
  │
  ├─ 디바운스: 5초
  ├─ 배치 처리 → 해당 경로 upsert/delete
  ├─ 이중 캐싱: positive 캐시 (새 디렉토리) + negative 캐시 (scan_root 외부)
  │
  └─ 30초마다 usn_next를 meta 테이블에 flush
```

### Windows: RDCW Fallback Watcher (`win/rdcw_watcher.rs`)

```
RDCW watcher (fallback — USN 사용 불가 시)
  │
  ├─ notify 크레이트 사용 (ReadDirectoryChangesW 래퍼)
  ├─ 감시 루트: C:\
  ├─ 디바운스: 300ms
  │
  ├─ 이벤트 처리: Create, Delete, Modify, Rename
  ├─ rename 페어링 (500ms 타임아웃)
  ├─ 30초마다 rdcw_last_active_ts 저장 (재시작 시 오프라인 catchup용)
  │
  └─ ~1초당 1회 폴링 (CPU 거의 0%)
```

---

## Native UI APIs

GPUI calls Rust search/file/settings functions directly through worker tasks.
There are no Tauri command macros, State injection or WebView IPC messages.
Rename/trash update the filesystem; the daemon/service alone writes the shared
index. See [GPUI architecture](gpui-architecture.md).

## Backend Events

| Event | Payload | 시점 |
|-------|---------|------|
| `index_progress` | `{ scanned, indexed, currentPath }` | 인덱싱 중 200ms 간격 |
| `index_state` | `{ state, message, isCatchup }` | Indexing/Ready/Error 전환 시 |
| `index_updated` | `{ entriesCount, lastUpdated, permissionErrors }` | 인덱싱 완료, watcher 업데이트, 파일 액션 후 |
| `context_menu_action` | 액션 페이로드 | Windows: 네이티브 컨텍스트 메뉴 액션 결과 |
| `focus_search` | (없음) | Cmd+Shift+Space 글로벌 단축키 (macOS) |

---

## Native UI Architecture

See [GPUI architecture](gpui-architecture.md) for the current source layout,
input/IME handling, generation guards, virtual list, dialogs, preferences,
status bridge and native packaging. See [test cases](gpui-test-cases.md) and
[performance](gpui-performance.md) for measured behavior.

## 플랫폼 비교

| 기능 | macOS | Windows |
|------|-------|---------|
| 스캔 범위 | `$HOME` | `C:\` (전체 드라이브) |
| 인덱싱 | jwalk 증분 (2-pass depth) | MFT 스캔 (NTFS 메타데이터, rayon 병렬) → WalkDir fallback |
| 파일 워처 | FSEvents (fsevent-sys 직접 바인딩) | USN 저널 → RDCW fallback |
| 재시작 시 이어하기 | FSEvent replay (저장된 event_id) | USN resume (저장된 next_usn) |
| 검색 fallback | Spotlight (mdfind) | N/A |
| 인메모리 인덱스 | N/A | MemIndex (MFT→DB upsert 중) |
| 아이콘 로딩 | NSWorkspace (확장자 기반, 프리워밍) | IShellItemImageFactory (exe/lnk 파일별) + SHGetFileInfo |
| 컨텍스트 메뉴 | 커스텀 (프론트엔드) | 네이티브 Explorer 컨텍스트 메뉴 (Shell API) |
| 글로벌 단축키 | Cmd+Shift+Space (global-hotkey) | 미등록 |
| 윈도우 효과 | GPUI window with the original custom titlebar | 테마별 배경색 |
| Quick Look | 지원 (Space 키) | N/A |
| Full Disk Access | FDA 배너 확인 | N/A |
| 클립보드 | `pbcopy` (경로), NSPasteboard (파일) | `cmd /C clip` |
| 파일 열기 | `open` 명령 | `cmd /C start ""` |
| 파일 위치 표시 | `open -R` | `explorer /select,` |

---

## Key Constants

| 상수 | 값 | 위치 |
|------|---|------|
| `DEFAULT_LIMIT` | 300 | 검색 기본 결과 수 |
| `SHORT_QUERY_LIMIT` | 100 | 1자 쿼리 결과 제한 |
| `MAX_LIMIT` | 1,000 | 최대 결과 수 |
| `BATCH_SIZE` | 10,000 | DB 배치 쓰기 단위 (macOS 인덱싱) |
| `MFT_BATCH_SIZE` | 50,000 | DB 배치 쓰기 단위 (Windows MFT) |
| `SHALLOW_SCAN_DEPTH` | 6 | Pass 0 최대 depth (macOS) |
| `jwalk_num_threads()` | available/2 (4–16) | 동적 병렬 워커 수 |
| `WATCH_DEBOUNCE` | 300ms | 파일 변경 디바운스 (macOS FSEvents, Windows RDCW) |
| `USN_CHANGE_DEBOUNCE` | 5s | USN watcher 디바운스 (Windows) |
| `RENAME_PAIR_TIMEOUT` | 500ms | rename 이벤트 페어링 타임아웃 (Windows USN/RDCW) |
| `RECENT_OP_TTL` | 2s | rename/trash 중복 방지 |
| `NEGATIVE_CACHE_TTL` | 60s | 0건 검색어 캐시 |
| `SPOTLIGHT_TIMEOUT` | 3s | mdfind 타임아웃 (macOS) |
| `SPOTLIGHT_MAX_RESULTS` | 300 | mdfind 최대 결과 (macOS) |
| `WSEARCH_TIMEOUT` | 10s | Windows Search 서비스 타임아웃 |
| `MUST_SCAN_THRESHOLD` | 10 | replay 중 full scan 트리거 (macOS) |
| `EVENT_ID_FLUSH_INTERVAL` | 30s | event_id / usn_next DB 저장 주기 |
| `STATUS_EMIT_MIN_INTERVAL` | 2s | 상태 업데이트 스로틀 |
| `DB_BUSY_RETRY_DELAY` | 3s | DB busy 시 재시도 지연 |
| Search debounce | 200ms leading + trailing | `ui/mod.rs` |
| Page size | 500 / 한 글자 100 | `ui/mod.rs` |
| Row height | 27px | GPUI uniform_list |
| Native icon cache | 512 | 프론트엔드 아이콘 캐시 제한 |

## macOS 데몬-writer 아키텍처

설계 문서: `design-macos-daemon-writer.html` (v3). 이 전환 이후 macOS는
Windows 서비스 모델과 동일하게 동작한다: 상주 데몬이 **유일한 index.db
writer**이고 GUI/MCP는 순수 reader다.

- **자동 실행**: SMAppService(macOS 13+)로 등록되는 launchd LaunchAgent.
  plist는 번들 내장(`Contents/Library/LaunchAgents/com.everything.app.daemon.plist`,
  `RunAtLoad`, `KeepAlive.SuccessfulExit=false`, `EVERYTHING_LAUNCHD=1`)이며
  시스템 설정 > 로그인 항목에 표시된다. 앱 시작 시 자가치유 등록 — 내장
  plist 바이트가 `agent_plist.marker`와 다를 때만 재등록. `register()`는
  즉시 기동이므로 온보딩 결정 기록에 게이트된다.
- **생명주기** (`daemon.rs`): Starting → WaitingOnboarding(첫 설치: 결정
  기록 전 빌드·TCC 접근 없음; 기존 설치는 자동 통과; MCP 전용 설치는
  `mcp.activity` 7일 경과 시 탈출) → Building → Ready → Quiescing.
  종료코드가 launchd 계약: 0 = 다음 로그인/kickstart까지 휴면, 1 = 디스크
  바이너리로 재기동. SIGTERM(로그아웃·종료·등록해제)은 quiesce: 패스 취소
  ≤2s → 워처 join ≤3s → `wal_checkpoint(TRUNCATE)` best-effort. 5초 감독
  틱: `index_complete` 치유, 죽은 워처 재기동, FDA 프로브(TCC.db 1바이트
  read, 미보유 시 60s 주기; rising edge → 강제 catchup), 실행 파일 identity
  감시(교체 → exit 1, 3틱 소실 → 휴면).
- **제어 IPC** (`daemon_ipc.rs`): UDS `<data dir>/daemon.sock`, 0600 +
  getpeereid, 개행 JSON, 연결당 1건. 명령: `status`, `reset_index`(600s
  쿨다운), `restart`, `disable`, `attach_lease`. 시작 빌드 전에 오픈;
  경로가 길면 `$TMPDIR` 폴백 + `daemon.sock.path` 사이드카.
- **read-only GUI** (`mac/bridge.rs`): 캐시 카운트로 즉시 Ready 시드, 1Hz
  status 폴링으로 변화 시에만 `index_*` 재방출(비-fresh Building →
  `"Catchup: …"` = `isCatchup`), 전용 커넥션으로 `PRAGMA data_version` 감시
  (캐시 무효화 + 검색 풀 폐기 + `fts_ready` 재판정), 연결 실패를
  `service_health` 상태로 분류(`starting`/`stopped`/`unresponsive`/
  `version_mismatch`/`not_registered`/`approval_required`/`disabled`)하고
  국면당 1회 단일 규칙으로 데몬을 살린다: 등록됨 → `launchctl kickstart`,
  opt-out/OS OFF → `--lease` 스폰, dev 빌드 → detached 스폰. GUI의
  rename/trash는 파일시스템 연산만 수행하고 데몬 워처가 반영한다.
- **opt-out lease 모드**: 자동 실행을 끄면 opt-out 기록(데몬 시작 관문이
  SMAppService 사용자 OFF 상태와 함께 검사) → 데몬 정지 → 등록 해제.
  꺼진 동안 GUI 브리지와 MCP 세션이 60s `attach_lease` 갱신(TTL 5분)으로
  `--lease` 데몬을 유지하고, 마지막 이용자가 소멸하고 `gui.lock`이 없으면
  데몬이 스스로 종료한다.
- **S5 rename rescan**: 외부 디렉터리 rename은 자식별 FSEvents가 없으므로,
  rename 플래그가 붙은 디렉터리와 "디스크에 자식이 있는데 DB 자손이 없는"
  신규 디렉터리에 대해 스트리밍 서브트리 rescan을 큐잉한다(전용 7s 쿨다운;
  MustScanSubDirs는 300s 유지).
- **검증**: `scripts/verify_macos_daemon.sh`(자동화 subset)와
  `docs/macos-daemon-writer-verification.md`(26항목 체크리스트). 릴리스는
  반드시 `scripts/build_dmg.sh`(서명+공증) 경유.
