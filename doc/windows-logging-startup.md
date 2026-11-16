# Windows 로그 설정과 재색인 중 검색

Windows의 앱과 EverythingIndexer 서비스는 다음 파일을 함께 읽는다.

```ini
; C:\ProgramData\com.everything.app\config\setting.ini
logging=true
```

파일이나 키가 없으면 기본값은 `false`다. 설정 > **Log**의 On/Off 스위치도
같은 파일을 저장한다. 다른 설정값과 줄바꿈 형식은 유지하며 임시 파일을 작성한 뒤
교체하므로 설정 파일이 중간 상태로 남지 않는다. 변경은 실행 중인 각 프로세스에
약 0.5초 안에 반영되며, 열린 설정 메뉴도 갱신한다. 수동으로 켜거나 메뉴를 처음
사용하기 전까지 설정 파일을 자동 생성하지 않는다.

로그 위치는 다음과 같다. Off로 바꿔도 기존 로그는 보존한다.

- 앱: `%APPDATA%\com.everything.app\logs\everything.log`
- 서비스: `%ProgramData%\com.everything.app\data\logs\service.log`

각 줄에는 날짜·밀리초·시간대·프로세스 역할이 들어간다. 단계의 시작과 완료에는
`phase`, `event`, `elapsed_ms`를 기록하며, 긴 메타데이터 읽기와 DB 반영 단계는
처리 수량도 기록한다. 로그는 약 5 MiB에서 `.log.1`로 순환한다. 검색 로그에는
질의 길이·모드·정렬·결과 수·소요 시간을 기록하며 파일명이나 검색어를 추가하지 않는다.
치명적인 서비스 오류의 Windows 이벤트 로그 기록은 별도로 유지한다.

## 수정한 문제

1. Windows 브리지가 서비스 이벤트만 전송하고 `AppState.status`를 갱신하지 않아
   GPUI가 캐시된 Ready를 읽던 문제를 수정했다. 상태·처리 수량·현재 단계·오류를
   먼저 갱신한 다음 이벤트를 보낸다. 연결 전에는 캐시된 항목 수를 읽더라도
   Ready로 단정하지 않는다.
2. MFT 재색인 과정이 이름 검색 인덱스까지 제거해 첫 500개 목록이 수백만 행을
   전체 스캔하고 임시 정렬하던 문제를 수정했다. 이름 인덱스를 보존하고, 이전
   중단으로 인덱스가 없으면 메타데이터 처리 전에 복구한다. 나머지 보조 인덱스는
   기존처럼 재생성한다.

첫 검색을 기다리는 동안에는 본문과 상태 표시줄에 `Loading files · Ns`를 표시한다.
검색 결과가 없는 경우와 검색 오류도 서로 다른 문구로 표시한다.
구체적인 단계가 있으면 `Indexing` 대신 `Updating file database · 1,950,000 /
4,623,473 · 30s`처럼 한 번만 표시한다. 이때 중복 단계 문구와 `scanned` 수는
제거하고, 검색 결과 수는 별도로 유지한다. 400×300 창의 200% 확대에서도 로딩
문구가 표의 가로 스크롤 영역 밖으로 밀려나지 않도록 viewport 중앙에 배치했다.

## 기록하는 단계

- `service.prepare`, `service.open_database`: 서비스 준비와 DB 열기
- `mft.scan_records`, `mft.enumerate`, `mft.resolve_paths`: MFT 읽기와 경로 계산
- `mft.prepare_name_index`: 재색인 중에도 이름 검색이 가능한지 확인·복구
- `mft.read_metadata`: 디렉터리별 파일 크기·수정 시간 읽기와 처리 폴더 수
- `mft.upsert`: 파일 DB 반영 시간과 처리 항목 수
- `mft.cleanup`, `mft.secondary_indexes`, `mft.finalize`, `mft.complete`: 정리와 마무리
- `gui.setup`, `gui.backend`, `search.execute`, `search.count`, `search.complete`,
  `gui.first_results`: 앱 준비부터 첫 결과를 그린 시점까지의 시간

## 검증

원시 자료는 로컬 `.artifacts/windows-logging-startup/`에 보관한다.

- 전체 Rust 테스트: **151 통과**, 기본 제외 스트레스 벤치마크 8개.
- 실제 창에서 로그 설정·수동 변경·재시작·대기 화면: **6 통과**.
- 기존 검색·선택·삭제·클립보드·창 동작: **17 통과**.
- 트리·설정·50–200% 확대: **14 통과**.
- 설치 종료·백업 검사: x64/x86에서 각각 **9 통과**.
- `scripts\build-windows.ps1`로 0.6.4 기본 release 및 NSIS 설치파일 생성.

0.6.4의 트리 파일 동작 추가에 맞춰 트리 Shift+Delete가 트리의 파일만 삭제하고
결과 표의 다른 파일은 보존하는지도 확인했다. 클립보드는 UI의 쓰기가 끝난 뒤 읽는다.
지연 화면 검사는 격리된 `ui-automation` 실행에서만 5초 지연을 주입한다.
이 지연은 정상 빌드와 실제 데이터 실행에서는 적용되지 않는다.

실제 DB의 첫 실행 사전 확인에서는 500개가 **518ms**에 표시됐고, 실행 계획은
`SCAN entries USING INDEX idx_entries_name_nocase`였다. 이 사전 확인 시 기존
서비스는 Ready 상태였으며, 수정한 서비스의 재색인 검증과 구분한다.

수정한 서비스를 실제 설치한 뒤 약 462만 항목의 재색인 중에도 첫 500개가
**887ms**에 표시됐다. 실행 계획은 이름 인덱스를 사용하며 임시 정렬이 없었다.
서비스의 Indexing과 GUI 상태가 일치하고, 상세 단계가 앞쪽에 한 번만 표시되며
뒤쪽 메시지는 `null`임을 실제 창과 상태 관측으로 확인했다.

전역 Log Off 동안 서비스는 `Reading file metadata · 366804/449424 folders`에서
`Updating file database · 200000/4623607`까지 진행했지만 앱·서비스 로그 크기는
각각 8,952 / 19,674 bytes로 유지됐다. On으로 복귀한 뒤 양쪽 로그가 다시 증가했다.
검증 후 `setting.ini`는 `logging=true`로 남겼다.

첫 서비스 검증의 전체 462만 항목 재색인도 `mft.complete ... ok=true`로 완료됐고
`index_complete=1`을 읽어 확인했다. 단계별 기록은 MFT 읽기 17,708ms, 경로 계산
2,055ms, 검색 인덱스 준비 10,131ms, 메타데이터 8,849ms, DB 반영 272,957ms,
보조 인덱스 28,291ms이며 전체 패스는 360,894ms였다.
최종 통합 UI 설치 후의 두 번째 전체 패스도 260,291ms에 정상 완료됐다.
일반 사용자 환경설정으로 설치된 앱을 다시 열었을 때는 첫 500개가 457ms에
그려졌으며, 테스트용 설정 없이 실행한 앱을 열어 두었다.

- [실제 서비스·GUI 검증 결과](../.artifacts/windows-logging-startup/live-indexing-refined/live-results.json)
- [통합 상태 표시 화면](../.artifacts/windows-logging-startup/live-indexing-refined/latest-status.png)
- [전체 서비스 패스 로그](../.artifacts/windows-logging-startup/service-first-complete.log)
- [최종 Rust 검사](../.artifacts/windows-logging-startup/tests-ui-refined.log)
- [로그 기능과 로딩 화면 검사](../.artifacts/windows-logging-startup/logging-ui-refined/logging-results.json)
- [작은 창 200%의 로딩 표시](../.artifacts/windows-logging-startup/logging-ui-refined/06-small-loading-200.png)
- [최종 기존 동작 검사](../.artifacts/windows-logging-startup/regression-ui-final/ui-results.json)
- [최종 트리·확대 검사](../.artifacts/windows-logging-startup/features-ui-release/feature-results.json)
- [최종 서비스 로그](../.artifacts/windows-logging-startup/service-final.log)
- [일반 실행 기록](../.artifacts/windows-logging-startup/normal-launch.json)
- [로컬 설치파일 빌드 로그](../.artifacts/windows-logging-startup/windows-build-refined.log)

최종 설치파일은 [Everything_0.6.4_x64-setup.exe](../app/target/release/bundle/nsis/Everything_0.6.4_x64-setup.exe)다.

다음 스크립트는 테스트용 DB에서 앱을 직접 실행한다.

```powershell
python -X utf8 scripts/run_windows_logging_tests.py --binary .artifacts/windows-logging-startup/everything-ui-automation.exe --fixture .artifacts/windows-logging-startup/fixture --output .artifacts/windows-logging-startup/logging-ui-final
```

실제 서비스 검증은 별도 스크립트를 사용한다. 인덱스와 서비스를 초기화하지 않고,
GUI 환경설정을 별도 디렉터리에 저장한다. `--toggle-log`는 전역 로그 설정을
Off/On으로 실제 변경한 뒤 On으로 남긴다.

```powershell
python -X utf8 scripts/verify_windows_indexer_live.py --binary 'C:\Program Files\Everything\Everything.exe' --output .artifacts/windows-logging-startup/live-indexing --toggle-log --require-indexing --observe-seconds 30
```

현재 재색인 중인 서비스가 있어야 `--require-indexing` 검사를 통과한다. 실제 서비스
교체와 재색인 중 검색의 최종 결과는 해당 실행의 `live-results.json`으로 확인한다.
