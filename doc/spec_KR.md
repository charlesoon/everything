아래 문서는 Tauri(v2) + Svelte로 바로 개발에 들어갈 수 있도록 정리한 최종 설계/스펙(implementation-ready) 입니다. (Quick Look 제외, "검색 속도" 최우선, Enter=Rename, Double click=Open 반영)

---

## 0. 문서 정보

- **제품명(가칭):** Everything
- **플랫폼:** macOS, Windows
- **기술 스택:** Tauri v2 + Rust + Svelte
- **목표:** Everything급(체감) 초고속 파일/폴더 "이름 기반" 검색
- **UI 방향:** Everything(Windows) 클론 스타일 — 검색창 + 결과 테이블 중심의 단순하고 밀도 높은 인터페이스
- **윈도우 동작:** 일반 앱 윈도우 + 글로벌 단축키(macOS: Cmd+Shift+Space)로 즉시 활성화

---

## 1. 목표/비목표

### 1.1 목표

- 타이핑 즉시 결과 갱신(검색 응답 <50ms 체감)
- 대용량(50만~100만 엔트리)에서도 UI 멈춤 없이 동작
- Everything 같은 "검색창 + 즉시 필터링 리스트" 경험
- 항상 case-insensitive 검색
- 크로스 플랫폼 지원: macOS와 Windows에서 네이티브 플랫폼 기능 활용
- 필수 액션 제공:
  - Open
  - Open With… (MVP: Reveal in Finder/Explorer fallback)
  - Reveal in Finder / Explorer
  - Copy Path
  - Move to Trash / 휴지통
  - Rename(Enter)

### 1.2 비목표(이번 버전에서 하지 않음)

- 내용 검색(全文)
- Quick Look
- 네트워크/원격 드라이브 인덱싱
- App Store 샌드박스 완전 대응(추후 과제)
- 검색 필터(파일/폴더/확장자 필터) — MVP에서는 필터 없이 전체 검색만
- Linux 지원 (부분적 — xdg-open을 통한 기본 open/reveal/clipboard)

---

## 2. 핵심 UX 스펙

### 2.1 메인 화면 구성

- **상단:** 검색 입력창(앱 시작 시 자동 포커스)
- **중앙:** 결과 테이블(가상 스크롤)
  - Name (파일 아이콘 + 이름)
  - Path(Directory)
  - Kind(확장자/파일/폴더)
  - Modified
- **하단 상태바:**
  - Index status: Ready | Indexing | Error
  - Indexed entries count
  - Last updated timestamp

### 2.2 입력/조작 규칙(확정)

- **Double click:** Open
- **Enter(선택 상태, 편집 아님):** Rename(인라인 편집 시작)
- **Enter(편집 중):** Rename 확정
- **Esc(편집 중):** Rename 취소

다중 선택:
- **Shift+클릭:** 범위 선택
- **Cmd+클릭 (macOS) / Ctrl+클릭 (Windows):** 개별 토글 선택
- 다중 선택 시 가능한 액션: Open, Reveal in Finder/Explorer, Copy Path, Move to Trash
- Rename은 단일 선택 상태에서만 가능 (다중 선택 시 Rename 비활성화)

### 2.3 키보드 단축키(필수)

- `Cmd+Shift+Space` : 글로벌 단축키 — 앱 활성화/검색창 포커스 (macOS만)
- `↑/↓` : 선택 이동
- `Shift+↑/↓` : 범위 선택 확장
- `PageUp/PageDown` : 빠른 이동(선택)
- `Cmd+O` / `Ctrl+O` : Open
- `Cmd+Enter` / `Ctrl+Enter` : Reveal in Finder/Explorer
- `Cmd+C` / `Ctrl+C` : Copy Path
- `Del` 또는 `Cmd+Backspace` : Move to Trash (기본: 확인 다이얼로그 ON)
- `F2` : Rename(보조, Enter와 동일)
- `Cmd+A` / `Ctrl+A` : 전체 선택

Enter가 Rename이므로 `Cmd+O` / `Ctrl+O`를 "열기 기본 단축키"로 강제합니다.

### 2.4 우클릭 메뉴(필수)

**macOS (커스텀 메뉴):**
- Open
- Open With… → Reveal in Finder (MVP)
- Reveal in Finder
- Copy Path
- Move to Trash
- Rename (단일 선택 시에만 표시)

**Windows (네이티브 Explorer 컨텍스트 메뉴):**
- Open, Reveal in Explorer, Copy Path (기본 항목)
- Shell 컨텍스트 메뉴 항목 (연결 프로그램, 보내기 등)

---

## 3. 성능 목표(필수 SLO)

### 3.1 검색

- 입력 후 결과 응답(백엔드): p95 < 30ms
- UI 렌더 포함 체감: < 50ms
- 반환 결과 제한: 기본 limit=300 (설정 가능 100~1000)

### 3.2 인덱싱

- 초기 인덱싱은 백그라운드 수행, UI 프리즈 금지
- DB 쓰기는 batch transaction으로 처리
- 변경 감지(watcher)는 debounce + 부분 재스캔으로 안정성 우선

---

## 4. 아키텍처 개요

### 4.1 구성 요소

- **Frontend(Svelte):** UI, 입력 이벤트, 가상 스크롤, 컨텍스트 메뉴
- **Backend(Rust):**
  - 인덱서(스캔 + DB upsert)
    - macOS: jwalk 증분 2-pass 스캔
    - Windows: NTFS MFT 스캔 (rayon 병렬)
  - 검색 엔진(LIKE 기반 다중 인덱스 최적화)
  - 액션 수행(open/reveal/trash/rename)
  - 증분 업데이트용 watcher
    - macOS: FSEvents (fsevent-sys 직접 바인딩)
    - Windows: USN Change Journal → ReadDirectoryChangesW fallback
- **Storage(SQLite):**
  - `entries` 테이블(정규화 데이터)
  - 검색 모드별 다중 인덱스

### 4.2 데이터 흐름

1. 앱 시작 → 파일 시스템 스캔 → entries 테이블 채움
   - macOS: `$HOME` jwalk 스캔
   - Windows: `C:\` MFT 열거
2. 사용자가 검색 → Rust가 LIKE 기반 다중 모드 쿼리 → 상위 N개 반환
3. Svelte가 리스트 렌더
4. 파일 변경 발생 → watcher 큐 → (경로 단위) upsert/delete 반영
   - macOS: FSEvents
   - Windows: USN 저널 / ReadDirectoryChangesW

---

## 5. 데이터 저장소 설계(SQLite)

### 5.1 DB 파일 위치

- `AppDataDir/index.db` (Tauri app data dir)

### 5.2 스키마(확정)

**entries**
- `id` INTEGER PRIMARY KEY
- `path` TEXT NOT NULL UNIQUE (전체 경로)
- `name` TEXT NOT NULL (basename)
- `dir` TEXT NOT NULL (parent directory path)
- `is_dir` INTEGER NOT NULL (0/1)
- `ext` TEXT (lowercase extension, dir이면 NULL)
- `mtime` INTEGER (unix epoch seconds, optional)
- `size` INTEGER (optional, initial MVP에서는 저장해도 되고 생략 가능)
- `indexed_at` INTEGER NOT NULL
- `run_id` INTEGER NOT NULL DEFAULT 0

**Indexes:**
- `idx_entries_name_nocase` — `name COLLATE NOCASE` (prefix/contains 검색)
- `idx_entries_dir` — `dir` (PathSearch 디렉토리 범위)
- `idx_entries_dir_ext_name_nocase` — `(dir, ext, name)` (PathSearch + ext shortcut)
- `idx_entries_ext` — `ext` (ExtSearch)
- `idx_entries_ext_name` — `(ext, name)` (ExtSearch + 정렬)
- `idx_entries_mtime` — `mtime` (수정일 정렬)
- `idx_entries_run_id` — `run_id` (증분 인덱싱 stale row 삭제)

**meta 테이블:**
- `key TEXT PRIMARY KEY, value TEXT NOT NULL`
- 저장 항목: `last_run_id`, `last_event_id` (macOS), `usn_next` / `usn_journal_id` / `index_complete` (Windows)

---

## 6. 검색 설계(LIKE 쿼리/정렬)

### 6.1 기본 검색 모드(확정)

- 대소문자: 항상 case-insensitive (`COLLATE NOCASE`)
- 입력 패턴에 따른 쿼리 분류:
  - `*` 또는 `?` 포함 → glob-to-LIKE 변환
  - `*.ext` (단순 확장자) → 확장자 직접 조회
  - `/` 또는 `\` 포함 → 경로 검색 (dir 범위)
  - 그 외 → 이름 검색 (3-phase: 정확 → 접두사 → 포함)

### 6.2 컬럼 정렬(확정)

검색 결과의 정렬은 relevance 기반이 아닌, 순수 컬럼 정렬 방식을 사용한다.

지원하는 정렬 모드:
- Name ASC (기본값)
- Name DESC
- Modified ASC (오래된 순)
- Modified DESC (최신 순)

동작 규칙:
- 컬럼 헤더 클릭으로 정렬 전환 (ASC → DESC → ASC 토글)
- 현재 정렬 컬럼/방향을 헤더에 화살표(▲/▼)로 표시
- 정렬은 백엔드(SQL ORDER BY)에서 수행하여 성능 보장

### 6.3 짧은 입력 최적화(필수 정책)

- query 길이 0: 최근 항목/즐겨찾기(옵션) 또는 빈 화면
- query 길이 1: 기본은 검색 수행하되 limit 낮춤(예: 100) + UI 디바운스(50ms)
- query 길이 2 이상: 정상 limit(300)

---

## 7. 인덱싱 설계

### 7.1 인덱싱 루트(확정)

| 플랫폼 | 스캔 범위 | 비고 |
|--------|----------|------|
| macOS | `$HOME` | 홈 디렉토리만 |
| Windows | `C:\` | C 드라이브 전체 |

루트 선택 UI 없음 — 항상 플랫폼 기본값으로 인덱싱.

### 7.2 Full Scan(초기 인덱싱)

**macOS — jwalk 증분 2-pass:**
- Pass 0 (shallow): depth ≤ 6, 우선순위 디렉토리 먼저
- Pass 1 (deep): 무제한 depth, depth 6 이하만
- 10,000행 단위 batch transaction
- Upsert: `INSERT ... ON CONFLICT(path) DO UPDATE SET ...`

**Windows — MFT 스캔:**
- `FSCTL_ENUM_USN_DATA`로 NTFS Master File Table 직접 열거
- 2-pass: MFT 열거 → 경로 해석 (rayon 병렬)
- 50,000행 단위 batch transaction
- Fallback: MFT 사용 불가 시 jwalk 기반 스캔

진행 이벤트:
- 200ms마다 UI로 scanned_count, indexed_count, current_path 송신

### 7.3 증분 업데이트(watcher)

**macOS — FSEvents:**
- fsevent-sys 직접 바인딩 (notify 크레이트 미사용)
- 이벤트를 경로 단위로 모아서 debounce (300ms)
- 재시작 시 event ID replay 지원 (깨끗한 replay면 full scan 생략)
- 처리: 경로 존재 → upsert, 경로 없음 → delete

**Windows — USN Journal (주요):**
- `FSCTL_READ_USN_JOURNAL`로 NTFS Change Journal 모니터
- MFT 스캔의 FRN 캐시로 syscall 없이 경로 해석
- 필터: CREATE, DELETE, RENAME_OLD/NEW, CLOSE (메타데이터 변경만 스킵)
- rename 페어링: OLD_NAME + NEW_NAME 500ms 타임아웃
- 디바운스: 30초 (시스템 노이즈가 많아 더 긴 주기)

**Windows — ReadDirectoryChangesW (fallback):**
- USN 사용 불가 시 notify 크레이트 사용
- 감시 루트: `C:\`, 디바운스: 300ms
- 재시작 시 오프라인 catchup용 last_active 타임스탬프 저장

**Windows — 오프라인 catchup (search_catchup):**
- 이전 인덱스가 있는 상태에서 재시작: Windows Search 서비스 시도 (ADODB via PowerShell)
- Fallback: 최근 수정된 파일 대상 mtime 기반 WalkDir 스캔

### 7.4 제외 규칙(기본값 + 옵션)

기본 제외(초기값):
- `.git/`, `node_modules/`, `.Trash`, `.npm`, `.cache`, `__pycache__`, `.gradle`

플랫폼별 제외:
- macOS: `Library/Caches/`, `Library/Developer/CoreSimulator`, `Library/Logs`, TCC roots (~40개)
- Windows: `Windows/`, `Program Files/`, `$Recycle.Bin/`, `System Volume Information/`, `AppData/Local/Temp`, `AppData/Local/Microsoft`

옵션:
- `.pathignore` 파일 (프로젝트 루트 및 홈 디렉토리)
- `.gitignore` 규칙 (ignore 크레이트, depth 3)

---

## 8. 액션 설계(파일 조작)

### 8.1 Open

- 기본 앱으로 열기
- 다중 선택 시: 선택된 모든 항목을 각각 기본 앱으로 열기
- macOS: `open <path>`, Windows: `cmd /C start "" "<path>"`, Linux: `xdg-open`

### 8.2 Open With…(확정: Reveal in 파일 관리자 fallback)

- MVP에서는 Reveal in Finder/Explorer로 대체
- Windows: 네이티브 컨텍스트 메뉴에 Shell API를 통한 "연결 프로그램" 포함
- 향후: macOS LaunchServices로 추천 앱 목록 팝오버(Phase 2)

### 8.3 Reveal in Finder / Explorer

- 파일 관리자에서 해당 항목을 선택 상태로 열기
- 다중 선택 시: 각 항목의 부모 폴더를 열기
- macOS: `open -R`, Windows: `explorer /select,`, Linux: `xdg-open` 부모

### 8.4 Copy Path(확정: 다중 선택 지원)

- 경로를 클립보드에 복사
- 단일 선택: 해당 경로 1줄
- 다중 선택: 각 경로를 개행(LF, `\n`)으로 구분하여 복사
- macOS: `pbcopy`, Windows: `cmd /C clip`, Linux: `wl-copy` / `xclip` / `xsel`

### 8.5 Move to Trash

- 휴지통으로 이동 (`trash` 크레이트로 크로스 플랫폼 지원)
- 기본: 확인 다이얼로그 ON
- 다중 선택 시: "N개 항목을 휴지통으로 이동하시겠습니까?" 확인
- (Shift 누르면 확인 없이 삭제 같은 UX는 추후 옵션)

### 8.6 Rename (Enter)

Rename은 단일 선택 상태에서만 동작한다. 다중 선택 시 Enter/F2 무시.

Rename은 파일 시스템 변경 + DB 갱신 + watcher 중복 억제까지 포함합니다.

동작 정의:
- Enter → 인라인 편집
- 편집 중 Enter → 확정
- 확정 시:
  1. 새 이름 정합성 검사(빈 문자열 금지, 경로 구분자 금지)
  2. 충돌 검사(동일 dir에 동일 name 존재 여부)
  3. `fs::rename(old_path, new_path)` 실행
  4. DB 업데이트: entries.path/name/dir/ext 수정
  5. UI 업데이트: 선택 항목 path 갱신

확장자 선택 규칙(권장):
- 편집 시작 시 기본 선택 범위는 "확장자 제외"
  - 예: `report.pdf` → `report`만 선택 상태
- 폴더는 전체 선택

---

## 9. 중복 이벤트/레이스 방지(필수)

Rename/Trash/Open 등 앱이 직접 수행한 작업은 watcher 이벤트로도 재유입될 수 있습니다.

### 9.1 "최근 작업 캐시" (필수)

- Rust에 `recent_ops`(LRU/HashMap) 유지
- key: old_path/new_path, op_type, timestamp
- TTL: 2초
- watcher 이벤트 처리 시:
  - TTL 내 동일 op로 판단되면 무시/병합

이거 없으면 rename 후 "깜빡임"이나 "중복 삭제/업서트"가 자주 생깁니다.

---

## 10. Tauri Command API(확정)

### 10.1 Commands

- `get_index_status() -> IndexStatusDTO`
- `get_platform() -> String` ("windows", "macos" 등)
- `start_full_index()`
- `reset_index()`
- `search(query: String, limit: u32, sort_by: String, sort_dir: String) -> SearchResultDTO`
- `open(paths: Vec<String>)`
- `open_with(path: String)` (MVP: reveal_in_finder 호출)
- `reveal_in_finder(paths: Vec<String>)`
- `copy_paths(paths: Vec<String>) -> String` (개행 구분 경로)
- `move_to_trash(paths: Vec<String>) -> Result`
- `rename(path: String, new_name: String) -> Result<EntryDTO>`
- `get_file_icon(ext: String, path: Option<String>) -> Option<Vec<u8>>` (확장자/경로별 시스템 아이콘)
- `show_context_menu(paths: Vec<String>, x: f64, y: f64)` (Windows 전용 — 네이티브 Explorer 컨텍스트 메뉴)

### 10.2 Events(Backend → Frontend)

- `index_progress { scanned, indexed, current_path }`
- `index_state { state: Ready|Indexing|Error, message? }`
- `index_updated { entries_count, last_updated, permission_errors }`
- `focus_search` (macOS 글로벌 단축키)

DTO 최소 필드(성능):
- `EntryDTO { path, name, dir, is_dir, ext?, mtime?, size? }`

---

## 11. 프론트엔드(Svelte) 구현 스펙

### 11.1 상태 모델

- `query: string`
- `results: EntryDTO[]`
- `selectedIndices: Set<number>` (다중 선택 지원)
- `lastSelectedIndex: number` (Shift 선택 앵커)
- `editing: { active: boolean, path: string, draftName: string }`
- `indexStatus: IndexStatusDTO`
- `sortBy: 'name' | 'mtime'` (기본값: `'name'`)
- `sortDir: 'asc' | 'desc'` (기본값: `'asc'`)
- `platform: string` ("windows", "macos" 등)

### 11.2 입력 이벤트 처리(상태 머신)

- 검색창 onInput:
  - debounce 0~30ms(기본 0 권장)
  - `invoke('search', { query, limit, sort_by, sort_dir })`
- 리스트 키다운:
  - Enter:
    - 편집 중이면 rename 확정
    - 단일 선택이면 startRename()
    - 다중 선택이면 무시
  - Cmd+O / Ctrl+O: open(selected paths)
  - Cmd+Enter / Ctrl+Enter: reveal_in_finder
  - Cmd+C / Ctrl+C: copy_paths
  - Esc: 편집 취소
  - Double click row: open(path)
  - 클릭: 단일 선택
  - Shift+클릭: 범위 선택
  - Cmd+클릭 / Ctrl+클릭: 토글 선택
- 우클릭:
  - Windows: `invoke('show_context_menu', { paths, x, y })` (네이티브 Shell API)
  - macOS: 커스텀 프론트엔드 컨텍스트 메뉴

### 11.3 가상 스크롤(필수)

- 결과가 수백 개여도 부드럽게
- row 높이 고정(성능 위해)
- 아이콘/Kind 계산 캐시

### 11.4 인라인 Rename UI(필수)

- Name 컬럼이 input으로 전환
- 확장자 제외 선택(권장 구현)
- 에러 시 토스트 + 편집 유지

### 11.5 파일 아이콘(확정)

**macOS:**
- macOS 시스템 아이콘 사용 (NSWorkspace via `swift -e` 서브프로세스)
- 확장자별 캐시: 동일 확장자는 아이콘을 한 번만 로드하고 캐시
- 아이콘 크기: 16x16 (테이블 행 높이에 맞춤)
- 시작 시 20개 주요 확장자 프리워밍

**Windows:**
- 실행 파일 전용 아이콘: exe, lnk, ico, url, scr, appx
  - IShellItemImageFactory (32x32 PNG, 실제 파일 경로 필요)
- 확장자 기반 fallback: SHGetFileInfo
- 프리워밍 없음 (온디맨드 로딩)

**공통:**
- 캐시 키: 확장자 문자열 (예: "pdf", "txt", "app")
- 폴더: 별도 폴더 아이콘 1개 캐시
- 확장자 없는 파일: 기본 문서 아이콘 사용
- 프론트엔드에서 `Map<string, dataURL>` 형태로 아이콘 캐시 유지

### 11.6 컬럼 헤더 정렬 UI

- Name, Modified 컬럼 헤더 클릭 시 정렬 전환
- 현재 정렬 컬럼에 방향 표시: ▲(ASC) / ▼(DESC)
- Path, Kind 컬럼은 정렬 미지원

---

## 12. 에러 처리/복구

- **DB open 실패:**
  - "Reset index" 버튼(파일 삭제 후 재생성)
- **인덱싱 중 권한 오류:**
  - 해당 경로 skip + 상태바에 경고 카운트
- **rename/trash 실패:**
  - 사용자에게 오류 메시지(권한/존재하지 않음/충돌)
- **MFT 스캔 실패 (Windows):**
  - USN 전용 또는 RDCW watcher 모드로 fallback

---

## 13. 설정(옵션)

- limit(기본 300)
- 숨김 파일 포함
- 제외 패턴 편집 (`.pathignore`)
- Trash 확인 다이얼로그 on/off

---

## 14. 개발 순서(바로 구현용 체크리스트)

**Phase 0: 검색 MVP(가장 먼저)**
1. SQLite 초기화 + entries 스키마 + 인덱스
2. full scan 인덱서 (macOS: jwalk, Windows: MFT)
3. search command (LIKE 기반 다중 모드 + limit + ORDER BY)
4. Svelte UI(검색창+결과+가상스크롤+파일아이콘)
5. Double click open
6. 상태바 index status
7. 컬럼 헤더 정렬(Name/Modified)

**Phase 1: 액션 + 다중 선택 + Rename UX**
8. 다중 선택 UI (Shift/Cmd+클릭, Windows에서 Ctrl+클릭)
9. Reveal/Copy/Trash 구현 (다중 선택, 크로스 플랫폼 대응)
10. Enter=Rename(인라인 편집, 단일 선택만) + rename command + DB 갱신
11. recent_ops 캐시로 watcher 중복 대비
12. 글로벌 단축키(Cmd+Shift+Space) 등록 (macOS)

**Phase 2: Watcher**
13. macOS: FSEvents watcher 연결
14. Windows: USN 저널 watcher + RDCW fallback
15. debounce + path upsert/delete
16. 대량 변경 스트레스 테스트

**Phase 3: Windows 네이티브 기능**
17. Windows 네이티브 컨텍스트 메뉴 (Shell API)
18. 파일별 아이콘 로딩 (exe, lnk 등)
19. 오프라인 catchup (Windows Search 서비스 / mtime 스캔)
