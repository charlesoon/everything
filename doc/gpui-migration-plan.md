# GPUI UI 전환 계획 및 스펙 검토

## 기준점과 작업 범위

- 작업 브랜치: `gpui-ui`, 원본 작업 폴더와 분리된 Git worktree.
- Tauri 기준 커밋: `68e4ddf` (시작 당시 커밋되지 않은 소스 변경 포함). 원본 폴더는 수정하지 않는다.
- 검토 자료: `doc/spec_KR.md`, `doc/architecture_KR.md`, 현재 `src/App.svelte`와 Rust 명령/데몬 구현.
- **사용자 최종 요구:** Tauri WebView와 UI를 동일하게 유지한다. 기존 실행 소스와 캡처를 레이아웃·색·글꼴·동작의 기준으로 삼는다.
- 최종 UI는 GPUI 네이티브 윈도우에서 렌더링한다. Svelte, Vite, Tauri IPC, WebView를 실행 경로에서 제거한다.
- 기존 Rust 검색 엔진, SQLite v7 데이터, FSEvents/USN 인덱서, MCP/데몬 CLI, read-only GUI 원칙을 유지한다.

## 기능 대응과 결정

| 영역 | GPUI 요구사항 |
| --- | --- |
| 윈도우 | 기존 30px 제목줄/윈도우 제어, 이동·크기 조절, 위치 저장, 시스템 테마 추종, 검색 자동 포커스; 기본 600×380/min 400×300 |
| 검색 | 기존 200ms leading+trailing 비동기 검색, 오래된 응답 무시, 한 글자 100개/기본 500개, 페이지 추가 로딩, 결과 강조, 빈 입력 시 전체 인덱스 첫 페이지 표시 |
| 테이블 | Name/Path/Size/Modified, 가상 스크롤, 서버 정렬, 컬럼 너비 변경·저장, 파일 아이콘, Inter/JetBrains Mono 원본 폰트·CSS 토큰, 단일 상태바 |
| 선택 | 단일, Cmd/Ctrl 토글, Shift 범위, 전체 선택, 키보드·Page 이동, 재검색 시 경로 기반 선택 보존 |
| 파일 작업 | Open, Open With (Reveal fallback), Reveal, Copy Path/Files, 휴지통 확인·부분 실패, 인라인 Rename·검증, Quick Look, 패키지 내용 보기, 파일 드래그 |
| 키보드 | 스펙 §2.3: Cmd/Ctrl+F/O/Enter/C/A, Del/Cmd+Backspace, F2, Space, Esc, 방향키·Shift·Page, macOS Cmd+Shift+Space |
| macOS Enter | Enter=Open을 유지한다. 병합한 `86f525b`에 따라 macOS는 선택한 항목을 잠시 후 다시 클릭해 Rename하며, F2는 Windows에서만 사용한다. |
| 클립보드 | macOS는 기존 Cmd+C=Copy Files, Alt+Cmd+C=Copy Path; Windows는 Ctrl+C=Copy Path. 입력창의 일반 복사/붙여넣기는 유지한다. |
| 상태 | Ready/Indexing/Error, 개수·시간·권한 실패, 데몬/서비스 상태, 갱신 시 검색 결과 반영 |
| 온보딩/설정 | FDA 안내·설정 열기·닫기, 첫 실행 선택, 자동 실행, 데몬/서비스 복구, Add/Ignore, 기존 Reset 즉시 재색인, 앱 재시작 |
| 백엔드 | UI와 Rust 사이 직접 함수 호출·타입 전달, 백그라운드 작업, bounded/coalesced 상태 이벤트 |

## 구현 순서

1. 기준 Tauri release 빌드 보존, 격리된 테스트 데이터와 반복 가능한 측정 방법 준비.
2. Rust UI 런타임 의존성 분리: 명령 매크로·WebView 생명주기를 제거하고 검색/파일 작업/데몬 이벤트 API 연결.
3. GPUI 검색 입력(IME 포함), 결과 테이블, 선택·정렬·페이지 로딩, 윈도우/테마 구현.
4. 파일 액션·확인창·컨텍스트 메뉴·인라인 Rename·글로벌 단축키·설정/온보딩 구현.
5. 기본 빌드/실행·패키징·CI·사용 문서를 GPUI로 전환.
6. 같은 크기·데이터·테마에서 Tauri/GPUI 캡처 비교. 개발 완료 후 TC 문서 작성, 단위/검색 회귀 테스트 및 실제 실행 윈도우 조작·스크린샷 검증.
7. 같은 머신/동일 DB/동일 쿼리/release 빌드로 전후 메모리·CPU·시작/검색 지연 측정, 원시 결과와 보고서 작성.

## 검증 기준

- 파서·검색·인덱서·MCP 기존 테스트 및 새 UI 상태 회귀 테스트가 통과한다.
- 실제 GPUI 윈도우에서 검색, 정렬, 스크롤, 선택, 파일 작업, 확인창, 테마·설정의 TC를 수행한다. 파일 변경 테스트는 전용 fixture만 사용한다.
- 검색 문법: 이름/대소문자/한글/공백/확장자/glob/절대·상대 경로/디렉토리/빈 입력/결과 없음/정렬/페이지/빠른 연속 입력.
- 비교 시 GUI 메모리에 Tauri WebContent/GPU 자식 프로세스를 포함하고, 공용 인덱서 메모리는 별도로 기록한다. RSS와 가능한 경우 physical footprint를 구분한다.
- backend-only 시간, 입력→결과 프레임 시간(디바운스 포함), 프로세스 시작→첫 프레임을 구분해 반복 측정한다. p50/p95·표본 수·환경·한계를 기록한다.
- macOS는 이 머신에서 실제 테스트한다. Windows는 가능한 빌드/테스트 범위와 네이티브 UI 검증 여부를 명확히 기록한다.

## 산출물

- GPUI 구현과 실행/패키징 명령.
- `doc/gpui-test-cases.md`: TC 및 실행 결과.
- `doc/gpui-performance.md`: 측정 방법·원시 데이터 위치·전후 결과.
- `doc/gpui-evidence/`: UI 스크린샷과 요약 JSON (DB·로그·빌드 산출물 제외).
