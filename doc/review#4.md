# Review #4 - 이슈 우선순위 정리

> 참고: 입력 메시지에서 원문이 `[Pasted Content 4429 chars]`로만 표시되어, 현재 워크트리 기준으로 재검토해 정리했습니다.

## P0 (Critical)

### 1) Live(fd) 검색 캐시 무효화 누락으로 stale 결과가 계속 노출됨
- 근거:
  - `fd_search`는 캐시 적중 여부를 `query/sort/ignore_fingerprint`만으로 판단함 (`src-tauri/src/main.rs:2593`).
  - 파일 변경 시 무효화되는 것은 `live_search_cache`뿐이며 (`src-tauri/src/main.rs:321`, `src-tauri/src/main.rs:1864`, `src-tauri/src/main.rs:2465`, `src-tauri/src/main.rs:2535`), `fd_search_cache`는 초기화되지 않음.
- 영향:
  - 파일/폴더 이름 변경, 휴지통 이동, 외부 변경 후에도 Live 모드에서 삭제된 경로가 재노출될 수 있음.
  - `open`/`reveal` 시 실패 경로를 사용자에게 보여주는 회귀로 이어질 수 있음.
- 권장 조치:
  - 캐시 무효화 함수를 통합해 `live_search_cache`와 `fd_search_cache`를 함께 비우기.
  - watcher 변경 처리 경로(`process_watcher_paths`)와 `rename`/`move_to_trash`/`reset_index`/풀 인덱싱 시작 시 공통 무효화 적용.

## P1 (High)

### 2) Live 모드에서 결과가 0건일 때 정렬 클릭이 DB 검색으로 전환되는 모드 누수
- 근거:
  - 정렬 클릭 핸들러가 `searchMode === 'live' && results.length > 0`일 때만 `runLiveSearch()`를 호출하고, 그 외에는 `runSearch()`를 실행함 (`src/App.svelte:591`).
- 영향:
  - Live 모드 UI 상태를 유지한 채 DB 결과가 표시될 수 있어 동작 일관성이 깨짐.
  - 특히 Live 모드 진입 직후(결과 0건) 정렬 클릭만으로 DB 결과가 나타나는 혼동이 발생함.
- 권장 조치:
  - `searchMode === 'live'`이면 결과 개수와 무관하게 `runLiveSearch()`만 호출하도록 분기 수정.

### 3) 검색 핫패스에서 무제한 로그 파일 append 수행
- 근거:
  - `search` 요청마다 `log_search()`가 호출되어 파일 append를 수행함 (`src-tauri/src/main.rs:2075`, `src-tauri/src/main.rs:2140`, `src-tauri/src/main.rs:2301`).
- 영향:
  - 타이핑 기반 검색에서 I/O가 지속 발생하고, `search.log`가 무제한 증가함.
  - 장시간 사용 시 디스크 사용량 증가 및 검색 지연 리스크가 있음.
- 권장 조치:
  - 기본 비활성화(디버그/옵트인) 또는 로그 로테이션(크기/개수 상한) 적용.

## P2 (Medium)

### 4) 사용자 홈 경로 하드코딩
- 근거:
  - `homePrefix`가 특정 사용자 경로로 고정되어 있음 (`src/App.svelte:9`).
- 영향:
  - 다른 계정/환경에서 `~` 경로 표시가 깨짐.
  - 저장소 가이드의 "사용자 종속 경로 하드코딩 금지" 원칙과 상충됨.
- 권장 조치:
  - 런타임 API(백엔드에서 홈 디렉터리 전달)로 값 주입.

### 5) 미사용 라이브 검색 코드/이벤트가 누적되어 경고 발생
- 근거:
  - `cargo check`/`cargo test`에서 `live_search` 관련 dead code 경고가 반복됨 (`src-tauri/src/main.rs:93`, `src-tauri/src/main.rs:1073`, `src-tauri/src/main.rs:1227`, `src-tauri/src/main.rs:1279`, `src-tauri/src/main.rs:1326`).
- 영향:
  - 유지보수 복잡도 증가, 실제 동작 경로 파악 난이도 상승.
  - 향후 `-D warnings` 환경에서 빌드 실패 가능성.
- 권장 조치:
  - 사용하지 않는 경로 제거 또는 실제 사용 경로로 통합.

## 검증 메모
- `npm run lint`: 성공 (Rust dead code warning 5건)
- `npm run test`: 성공 (`cargo test --no-run`)
- `cargo test --manifest-path src-tauri/Cargo.toml`: 성공 (39 passed)
