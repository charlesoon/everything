# Architecture

Everything — 파일 시스템 전체를 SQLite에 인덱싱하여 sub-50ms 파일/폴더 이름 검색을 제공하는 Tauri v2 데스크톱 앱. macOS와 Windows를 지원.

## Tech Stack

| Layer | Technology |
|-------|-----------|
| App framework | Tauri v2 |
| Backend | Rust (rusqlite, jwalk, ignore) |
| Frontend | Svelte 5 (단일 컴포넌트) |
| DB | SQLite WAL mode, LIKE 기반 검색 |
| Build | Vite 7, Cargo |
| macOS watcher | fsevent-sys (FSEvents 직접 바인딩) |
| Windows 인덱서 | MFT 스캔 (Win32 FSCTL), USN 저널, ReadDirectoryChangesW fallback, WalkDir non-admin fallback |
| Windows 추가 | windows 0.58, notify, rayon, png |
| Plugins | tauri-plugin-global-shortcut, tauri-plugin-drag, tauri-plugin-window-state, tauri-plugin-decorum |

---

## Module Structure

```
src-tauri/src/
├── main.rs              # 앱 상태, DB, 인덱싱, 검색, 파일 액션, IPC 핸들러
├── query.rs             # 검색 쿼리 파서 (SearchMode 분류)
├── fd_search.rs         # jwalk 기반 라이브 파일시스템 검색
├── mem_search.rs        # 인메모리 컴팩트 엔트리 검색 (MemIndex)
├── gitignore_filter.rs  # 지연 .gitignore 탐색 및 매칭
│
├── mac/                 # macOS 전용 모듈
│   ├── mod.rs
│   ├── fsevent_watcher.rs   # FSEvents 직접 바인딩
│   └── spotlight_search.rs  # mdfind 기반 Spotlight 검색 fallback
│
└── win/                 # Windows 전용 모듈
    ├── mod.rs               # Windows 인덱싱 오케스트레이션 (MFT → USN → RDCW fallback)
    ├── mft_indexer.rs       # NTFS Master File Table 스캔 (rayon 병렬)
    ├── nonadmin_indexer.rs  # WalkDir fallback (MFT 접근 불가 시)
    ├── usn_watcher.rs       # USN Change Journal 모니터
    ├── rdcw_watcher.rs      # ReadDirectoryChangesW fallback (notify 크레이트)
    ├── search_catchup.rs    # 오프라인 동기화 (Windows Search 서비스 / mtime 스캔)
    ├── icon.rs              # IShellItemImageFactory + SHGetFileInfo 아이콘 로딩
    ├── context_menu.rs      # 네이티브 Explorer 컨텍스트 메뉴 (Shell API)
    ├── volume.rs            # NTFS 볼륨 핸들 및 USN 저널 쿼리
    ├── path_resolver.rs     # FRN (File Reference Number) → 경로 변환
    └── com_guard.rs         # COM 초기화/정리 래퍼

src/
├── main.js              # Svelte 마운트 포인트
├── App.svelte           # 전체 UI (단일 컴포넌트)
└── search-utils.js      # 검색 디바운스 & 뷰포트 보존 유틸리티
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

모든 필드가 `Arc`로 래핑되어 `Clone` 가능. Tauri `State<AppState>`로 IPC 핸들러에 주입.

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

## IPC Commands

| Command | 방향 | 설명 |
|---------|------|------|
| `get_index_status` | FE→BE | 인덱싱 상태, 엔트리 수, 진행률 |
| `get_home_dir` | FE→BE | 홈 디렉토리 경로 |
| `get_platform` | FE→BE | `"windows"`, `"macos"` 등 반환 |
| `start_full_index` | FE→BE | 전체 재인덱싱 트리거 |
| `reset_index` | FE→BE | DB 초기화 후 재인덱싱 |
| `search` | FE→BE | DB 검색 → `SearchResultDto { entries, modeLabel, totalCount, totalKnown }` |
| `fd_search` | FE→BE | jwalk 라이브 검색 → `FdSearchResultDto { entries, total, timedOut }` |
| `open` | FE→BE | 파일 열기 (macOS: `open`, Windows: `cmd /C start`, Linux: `xdg-open`) |
| `open_with` | FE→BE | 파일 관리자에서 보기 |
| `reveal_in_finder` | FE→BE | macOS: `open -R`, Windows: `explorer /select,`, Linux: `xdg-open` 부모 |
| `copy_paths` | FE→BE | 경로 클립보드 복사 (macOS: `pbcopy`, Windows: `clip`) |
| `copy_files` | FE→BE | 파일 클립보드 복사 (macOS 전용, NSPasteboard) |
| `move_to_trash` | FE→BE | 휴지통 이동 + DB 삭제 |
| `rename` | FE→BE | 이름 변경 + DB 갱신 → 새 EntryDto 반환 |
| `get_file_icon` | FE→BE | 확장자/경로별 시스템 아이콘 PNG 반환 |
| `show_context_menu` | FE→BE | 네이티브 컨텍스트 메뉴 (Windows: Explorer Shell API, macOS: 커스텀) |
| `quick_look` | FE→BE | Quick Look 미리보기 (macOS 전용) |
| `check_full_disk_access` | FE→BE | 전체 디스크 접근 권한 확인 (macOS 전용) |
| `open_privacy_settings` | FE→BE | 개인 정보 설정 열기 (macOS 전용) |
| `set_native_theme` | FE→BE | 네이티브 윈도우 테마 설정 (dark/light) |
| `mark_frontend_ready` | FE→BE | 프론트엔드 초기화 완료 신호 |
| `frontend_log` | FE→BE | 프론트엔드 디버그 로깅 |

## Backend Events

| Event | Payload | 시점 |
|-------|---------|------|
| `index_progress` | `{ scanned, indexed, currentPath }` | 인덱싱 중 200ms 간격 |
| `index_state` | `{ state, message, isCatchup }` | Indexing/Ready/Error 전환 시 |
| `index_updated` | `{ entriesCount, lastUpdated, permissionErrors }` | 인덱싱 완료, watcher 업데이트, 파일 액션 후 |
| `context_menu_action` | 액션 페이로드 | Windows: 네이티브 컨텍스트 메뉴 액션 결과 |
| `focus_search` | (없음) | Cmd+Shift+Space 글로벌 단축키 (macOS) |

---

## Frontend Architecture

### 단일 컴포넌트 (`App.svelte`)

검색 입력, 가상 스크롤 테이블, 인라인 이름 변경, 컨텍스트 메뉴, 키보드 단축키, 아이콘 캐시, 상태 바, 테마 토글, Full Disk Access 배너(macOS)를 포함하는 단일 컴포넌트.

### 플랫폼 감지

시작 시 `get_platform()` 호출. 결과를 `platform` 변수에 저장하여 조건부 동작에 사용 (예: Windows 네이티브 컨텍스트 메뉴 vs macOS 커스텀 컨텍스트 메뉴).

### 상태 관리

Svelte 5 리액티브 변수 사용 (store 미사용).

| 카테고리 | 주요 변수 |
|---------|----------|
| 검색 | `query`, `results`, `searchGeneration`, `dbLatencyMs`, `searchModeLabel`, `sortBy`, `sortDir`, `totalResults`, `totalResultsKnown` |
| 선택 | `selectedIndices` (Set), `selectionAnchor`, `lastSelectedIndex` |
| 편집 | `editing { active, path, index, draftName }` |
| 인덱싱 | `indexStatus { state, entriesCount, lastUpdated, permissionErrors, isCatchup, backgroundActive }`, `scanned`, `indexed`, `currentPath` |
| 가상 스크롤 | `scrollTop`, `viewportHeight`, `colWidths` |
| 캐시 | `iconCache` (Map, 최대 500), `highlightCache` (Map, 최대 300) |
| UI | `platform`, `isMaximized`, `showFdaBanner`, `theme`, `contextMenu`, `toast` |

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
  │    ├─ invoke('search', { query, limit: 500, offset: 0, sort_by, sort_dir, include_total: false })
  │    ├─ 응답: { entries, modeLabel, totalCount, totalKnown }
  │    ├─ results = entries
  │    ├─ searchModeLabel = modeLabel
  │    ├─ 뷰포트 보존 로직 (스크롤 위치)
  │    └─ 선택 경로 기반 복원
  │
  └─ 무한 스크롤
       스크롤 하단 10행 이내 → loadMore()
       → invoke('search', { offset: results.length })
       → results에 append
```

### 가상 스크롤

```
고정 행 높이: 26px
버퍼: 상하 10행 (총 overscan ~20행)
OverlayScrollbars로 커스텀 스크롤바 스타일링

scrollTop
  → startIndex = max(0, floor(scrollTop / 26) - 10)
  → endIndex = min(results.length, startIndex + visibleCount)
  → visibleRows = results.slice(startIndex, endIndex)
  → translateY = startIndex * 26

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
| `Escape` | 선택 해제, 검색 입력 포커스 / 이름 변경 취소 |
| `↑` / `↓` | 선택 이동 (Shift: 범위 선택) |
| `PageUp` / `PageDown` | 페이지 단위 이동 |
| `Enter` | 열기 (Windows) / 인라인 이름 변경 시작 (macOS) |
| `F2` | 인라인 이름 변경 시작 |
| `Space` | Quick Look (macOS) |
| `Cmd+O` / `Ctrl+O` | 선택 항목 열기 |
| `Cmd+Enter` / `Ctrl+Enter` | Finder/Explorer에서 보기 |
| `Cmd+C` / `Ctrl+C` | 경로 복사 |
| `Cmd+A` / `Ctrl+A` | 전체 선택 |
| `Cmd+F` / `Ctrl+F` | 검색 입력 포커스 |
| `Delete` / `Cmd+⌫` | 휴지통 이동 |

### 컨텍스트 메뉴

| 플랫폼 | 구현 |
|--------|------|
| Windows | 네이티브 Explorer 컨텍스트 메뉴 (Shell API, `show_context_menu` 커맨드), `context_menu_action` 이벤트로 액션 수신 |
| macOS | 커스텀 메뉴: Open, Quick Look, Open With, Reveal in Finder, Copy Files, Copy Path, Move to Trash, Rename (단일 선택 시) |

### 아이콘 시스템

```
visibleRows 변경
  → ensureIcon(entry) 호출
  → iconCache에 있으면 즉시 반환 (최대 500개)
  → 없으면 invoke('get_file_icon', { ext, path })

macOS:
  → swift -e NSWorkspace 16x16 PNG (확장자 기반)
  → 시작 시 20개 주요 확장자 프리워밍

Windows:
  → 실행 파일 전용 아이콘: exe, lnk, ico, url, scr, appx
      IShellItemImageFactory (16x16 PNG, 실제 파일 경로 필요)
  → 확장자 기반 fallback: SHGetFileInfo
  → 프리워밍 없음 (온디맨드 로딩)
```

### 테마

시스템 설정 연동 (`prefers-color-scheme: dark`). 테마 버튼으로 토글. CSS 커스텀 프로퍼티 및 `data-theme` 속성 기반. localStorage에 저장.

```css
:root {
  --bg-app, --text-primary, --surface, --row-hover,
  --row-selected, --border-soft, --focus-ring, ...
}
```

### 상태 바

| 상태 | 표시 내용 |
|------|----------|
| 인덱싱 중 (엔트리 있음) | 펄싱 `●` + 진행률% + 경과 시간 + 엔트리 수 |
| 인덱싱 중 (엔트리 없음) | `인덱싱 시작 중...` |
| Ready | 녹색 `●` + 엔트리 수 + 인덱싱 소요 시간 |
| Ready (백그라운드 활성) | 펄싱 녹색 `●` |
| 검색 완료 | `"query" Xms · N results` |

### Full Disk Access 배너 (macOS)

macOS에서 시작 시 `check_full_disk_access()` 확인. 미부여 시 개인정보 설정 링크가 포함된 해제 가능한 배너 표시.

---

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
| 글로벌 단축키 | Cmd+Shift+Space (tauri-plugin-global-shortcut) | 미등록 |
| 윈도우 효과 | Vibrancy (NSVisualEffectMaterial) | 테마별 배경색 |
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
| `SEARCH_DEBOUNCE_MS` (FE) | 200ms | 프론트엔드 검색 디바운스 |
| `PAGE_SIZE` (FE) | 500 | 프론트엔드 페이지 크기 |
| `rowHeight` (FE) | 26px | 가상 스크롤 행 높이 |
| `ICON_CACHE_MAX` (FE) | 500 | 프론트엔드 아이콘 캐시 제한 |
| `HIGHLIGHT_CACHE_MAX` (FE) | 300 | 프론트엔드 하이라이트 캐시 제한 |
