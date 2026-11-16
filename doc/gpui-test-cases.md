# GPUI 전환 테스트

화면 기준은 Tauri 커밋 `68e4ddf`와 그 소스로 실행한 실제 창이다. UI를 새로 디자인하지 않고 글꼴, 색상, 배치와 메뉴를 유지한다. 이후 `macos-daemon-writer`의 `b2f2686`, `86f525b`를 병합하면서 macOS 이름 변경 제스처도 GPUI에 반영했다. macOS 26.7.3 / Apple M5 Pro에서 release 바이너리를 테스트했다.

## 자동화 방법

`ui-automation` 기능으로 빌드한 앱에 0600 Unix socket을 명시적으로 전달한다. 키·마우스는 AppKit NSEvent로, 확정 문자는 현재 키보드 언어에 영향을 받지 않는 NSTextInputClient `insertText:replacementRange:`로 전달해 GPUI의 실제 입력·포커스·hit testing을 거친다. UI 상태 JSON은 관측용이며 상태를 바꾸는 명령은 없다. 파일 작업은 마커가 있는 전용 fixture 안에서만 수행하고 원래 파일을 복원한다. FDA, 로그인 항목, 실제 데몬 등록과 Reset은 fixture 모드에서 실행하지 않는다.

```sh
python3 scripts/ui_fixture.py /tmp/everything-ui-fixture --entries 500000
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  cargo build --release --locked --features ui-automation --manifest-path app/Cargo.toml
python3 scripts/run_gpui_ui_tests.py --fixture /tmp/everything-ui-fixture \
  --output /tmp/everything-ui-results
```

실행 결과: **14 통과 / 0 실패**. 기계 판독 결과와 실행 시각은 ui-results.json에 있다.

| TC | 조작과 확인 | 결과 |
| --- | --- | --- |
| UI-01 | 시작 검색 포커스, 한글 확정 문자 입력, 입력 삭제 시 전체 파일 첫 500행 복원 | 통과 |
| UI-02 | 이름·대소문자·공백·한글·glob·절대/상대 경로·디렉토리·결과 없음 | 통과 |
| UI-03 | 빠른 쿼리 교체 시 오래된 결과 배제, 한 글자 100행 제한 | 통과 |
| UI-04 | 실제 PageDown 이동, 다음 페이지 로딩 | 통과 |
| UI-05 | 서버 정렬, Name/Path 경계 드래그, 정렬 버튼과 리사이저 hit testing | 통과 |
| UI-06 | Cmd/Shift 마우스 선택, Shift 방향키, 전체 선택, 실제 경로/파일 pasteboard | 통과 |
| UI-07 | 실제 macOS 네이티브 메뉴 열기, 방향키로 Copy Path 선택, clipboard 확인 | 통과 |
| UI-08 | 선택한 항목 다시 클릭으로 이름 변경, 확장자 제외 선택, 충돌·빈 이름·슬래시 오류, Enter/포커스 이탈 확정, 실제 파일 내용/경로 확인 | 통과 |
| UI-09 | 일반 Delete 기본 Cancel·Tab+Enter 확인, Cmd+Delete/Backspace 단일·다중 항목 즉시 휴지통 이동, 입력창 텍스트 삭제와 pending-trash 결과 억제 | 통과 |
| UI-10 | 앱에만 적용한 시스템 light/dark, fixture Reset 차단, 400×300 창과 확인창 | 통과 |
| UI-11 | Enter/Cmd+O 열기, Cmd+Enter 표시, Space Quick Look의 OS 요청 완료 | 통과 |
| UI-12 | 컬럼/창 크기 저장, 두 번 재시작 후 크기 증가 없음 | 통과 |
| UI-13 | 온보딩 키보드 포커스 유지, 나중에 선택, FDA 설정 버튼의 안내 변경과 배너 닫기 | 통과 |
| UI-14 | 시스템 더블클릭 간격 후 Rename, macOS F2 비활성, Escape·더블클릭 Open·입력창 포커스·창 비활성화 시 예약 취소 | 통과 |

UI-11은 요청 완료를 검증한다. 외부 편집기나 Quick Look 내용의 화면 검증으로 해석하지 않는다. 한글 TC는 확정 문자 입력이며 실제 한국어 IME 조합 중간 상태 검증은 아니다.

## 오른쪽 파일 트리

오른쪽 위 사이드바 버튼으로 파일 트리를 펼치거나 접는다. 파일 트리는 검색창 옆 공간까지 차지하고, 왼쪽 구분선은 타이틀바 맨 위까지 같은 위치로 이어진다. 타이틀바도 패널 너비에 맞춰 나뉘며 제목은 왼쪽 영역에 배치한다. 검색창과 결과 목록은 왼쪽 열에 배치한다. 왼쪽 검색 결과를 클릭하거나 키보드로 선택하면 해당 항목의 상위 폴더를 펼치고 선택 위치로 스크롤한다. 같은 항목을 다시 클릭해도 트리에서 다시 표시한다. 검색어와 관계없이 숨김·미인덱싱 파일을 포함한 실제 디렉토리 항목을 보여준다. 폴더는 백그라운드에서 한 단계씩 읽고, 화면에 보이는 행만 렌더링한다.

폴더 펼치기·접기, 상위 폴더 이동, 새로고침, 구분선 드래그로 너비 조절을 지원한다. 패널의 열림 상태와 너비를 저장하며 패널 토글로 검색 결과의 컬럼 비율이 바뀌지 않는다. 접근할 수 없거나 사라진 폴더에는 오류를 표시한다.

새로고침이나 창 복귀로 디렉토리를 다시 읽는 동안 기존 목록을 유지한다. 위쪽에 항목이 추가·삭제돼도 보고 있던 항목의 화면 위치를 보존한다. 검색 결과와 트리의 스크롤바는 내용이 영역을 넘치면 대기 상태에서도 표시한다. 결과 목록의 세로 스크롤바는 컬럼 헤더 아래에 고정하고, 가로 스크롤바는 결과 영역 하단에 고정한다.

하단 상태바의 선택 파일 경로와 트리 상단의 현재 폴더 경로를 클릭하면 전체 절대 경로를 복사한다. `~` 또는 잘린 경로로 표시돼도 원본 경로를 복사하며 `Copied path` 알림을 표시한다. 클릭 시 검색어·파일 선택·트리 탐색 위치는 유지된다.

```sh
python3 scripts/ui_fixture.py /tmp/everything-file-tree-fixture --entries 500000
MACOSX_DEPLOYMENT_TARGET=13.0 DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  cargo build --release --locked --features ui-automation --manifest-path app/Cargo.toml
python3 scripts/run_file_tree_ui_tests.py --fixture /tmp/everything-file-tree-fixture \
  --output .artifacts/file-tree-ui
python3 scripts/run_gpui_ui_tests.py --fixture /tmp/everything-file-tree-fixture \
  --output .artifacts/file-tree-regression
```

실제 GPUI/AppKit 입력을 통한 파일 트리·스크롤·경로 복사 **11 통과 / 0 실패**, 기존 UI 회귀 **14 통과 / 0 실패**. 새 기능 결과와 회귀 결과에 실행 기록이 있다.

| TC | 조작과 확인 | 결과 |
| --- | --- | --- |
| TREE-01 | 오른쪽 위 버튼으로 빈 패널 열기, 창 위치·크기 유지 | 통과 |
| TREE-02 | 검색창 옆까지 확장된 트리 위치, 선택 파일 표시, 320개 이상의 형제 항목 전체를 실제 디렉토리와 대조, 숨김·미인덱싱 파일 포함 | 통과 |
| TREE-03 | 미인덱싱 폴더 펼치기·접기, 빈 폴더, 24단계 깊은 폴더의 파일명 표시, 트리 키보드 이동과 별도 포커스 | 통과 |
| TREE-04 | 깊은 경로의 상위 폴더 자동 펼치기, 긴 목록의 선택 항목 자동 스크롤, Shift 선택 연동, light/dark | 통과 |
| TREE-05 | 타이틀바 높이에서 구분선 너비 조절 및 창 위치·크기 유지, 토글 후 원래 컬럼 복원, 400×300 창 | 통과 |
| TREE-06 | 파일 생성 후 새로고침, 이름 변경·휴지통 이동 후 열린 트리 갱신 | 통과 |
| TREE-07 | 상위 폴더 이동, 재시작 후 열림 상태·너비 복원, 다른 디렉토리의 결과 선택 | 통과 |
| TREE-08 | 폴더 접근 실패 표시와 복구, 기존 트리에서 누락된 경로의 오류 표시, 닫힘 상태 저장 | 통과 |
| TREE-09 | 위쪽에 파일이 추가된 후 새로고침, 창 숨김·복귀 후 선택 파일과 화면 위치 유지 | 통과 |
| TREE-10 | 3초 이상 대기 후 양쪽 세로·결과 가로 스크롤바 드래그, 독립 스크롤, 수동 탐색 중 새로고침 위치 유지, light/dark | 통과 |
| TREE-11 | 상태바·트리 경로 클릭 후 실제 클립보드의 전체 절대 경로 대조, 한글·공백·~ 표시·작은 창의 경로 잘림, 선택·검색어·트리 위치 유지 | 통과 |


최초 파일 트리 구현 시 `npm test -- --release --features ui-automation -j4`와 전체 Rust 테스트가 **142 통과 / 0 실패**했으며 트리 모델 테스트 6개가 포함됐다. 스크롤 수정 후에는 트리 모델 테스트 **7 통과 / 0 실패**를 확인했다. 오래된 비동기 응답 배제, 상위 경로 펼침, 범위 전환, 접힌 폴더 읽기 억제, 디스크 목록과 누락된 선택 경로, 새로고침 중 기존 목록·선택 유지 및 재갱신 전 응답 무효화를 검사한다. 기본 기능의 production release 빌드도 통과했다.

검증 요약에 명령, 소스·캡처 해시와 production 실행 파일 해시를 기록했다. 테스트는 마커가 있는 격리 fixture와 별도 설정 파일을 사용했다. Windows 실행 검증은 포함하지 않는다.

타이틀바까지 구분선을 확장한 후 UI 22건을 재실행해 모두 통과했다. 트리 목록의 시작 위치는 89px이며, 타이틀바 높이인 y=15에서 구분선을 드래그해 패널 너비만 바뀌고 창 위치·크기는 유지되는지 검증했다. 어두운·밝은 테마, 너비 조절 후 화면과 400×300 창의 캡처에서 구분선의 연속성과 제목·창 버튼 배치도 확인했다. 위 캡처와 UI 결과는 이 배치를 반영한다.

스크롤 수정 전에는 동일한 선택 항목의 트리 스크롤 값이 새로고침 후 -5367px에서 0으로 바뀌는 것을 재현했다. 기존 목록을 유지하며 다시 읽고, 첫 화면 항목을 기준으로 위치를 복원하도록 수정했다. 스크롤바는 자동 숨김을 끄고 목록 안에 고정해 잘리지 않도록 했다. 위 캡처와 최신 UI 결과에는 이 수정도 반영했다.

수정 후 UI 재검사 24건이 모두 통과했다. 첫 회귀 실행의 UI-02에서 `*.md` 검색이 12초 대기 시간을 한 번 초과했으며, 전체 재실행 14건과 같은 검색의 추가 10회 반복에서는 재현되지 않았다. 반복 검색은 약 0.24~0.27초였다. 실행 기록은 검증 요약의 `scrollingFix`에 보관했다.

경로 복사 추가 후 파일 트리 UI 11건과 기존 UI 14건이 통과했다. 첫 파일 트리 실행에서는 TREE-06의 `child.txt` 검색 대기가 한 번 초과했으며, 코드 변경 없이 전체 재실행한 11건은 모두 통과했다. 경로 복사 항목인 TREE-11은 두 실행에서 모두 통과했다. 실행 기록과 갱신한 0.6.1 패키지 검증은 검증 요약의 `pathCopy`에 보관했다.

macOS 최종 통합본에서 `npm test -- --release --features ui-automation -j4`가 성공했고, 전체 Rust 테스트는 **145 통과 / 0 실패 / 11 제외**였다. 파일 트리 UI 11건과 기존 UI 14건도 모두 통과했다. 최신 소스·패키지 검증 기록은 검증 요약의 `commitValidation`에 있다.

## 화면 배율

`Cmd/Ctrl`과 `+` 또는 `=`로 확대, `-`로 축소, `0`으로 100% 복원한다. 50~200% 범위에서 글꼴·아이콘·행·검색창·파일 트리·설정 화면을 함께 조절하며 재실행 후에도 유지한다. 타이틀바와 네이티브 창 버튼은 원래 크기를 유지한다. 컬럼과 트리 너비는 100% 기준 단위로 저장해 배율 변경과 재실행 시 비율을 보존한다.

```sh
MACOSX_DEPLOYMENT_TARGET=13.0 DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  cargo build --release --locked --features ui-automation --manifest-path app/Cargo.toml -j4
python3 scripts/run_ui_zoom_tests.py --fixture /tmp/everything-file-tree-validation \
  --output .artifacts/ui-zoom-native-final
python3 scripts/run_gpui_ui_tests.py --fixture /tmp/everything-file-tree-validation \
  --output .artifacts/ui-zoom-regression-final
python3 scripts/run_file_tree_ui_tests.py --fixture /tmp/everything-file-tree-validation \
  --output .artifacts/ui-zoom-file-tree-confirmed
```

| TC | 조작과 확인 |
| --- | --- |
| ZOOM-01 | 검색 입력 중 Command/Control, `=`, Shift+`+`, 축소·초기화, 일반 문자 입력 및 창 크기 유지 |
| ZOOM-02 | 150%에서 검색·선택·탐색 유지, 확대된 행 클릭과 상태바·트리 경로의 실제 클립보드 복사 |
| ZOOM-03 | 파일명 편집 중 확대·초기화 시 편집 상태와 파일명 유지 |
| ZOOM-04 | 트리 포커스·선택·스크롤 위치 보존, 125%에서 타이틀바 구분선과 컬럼 너비 조절 |
| ZOOM-05 | 반복 입력으로 50%·200% 한계 확인, 400×300 창과 설정 스크롤, 100% 복원 |
| ZOOM-06 | 재실행 후 배율·컬럼·트리 너비 복원, 테마 변경 및 설정 화면에서 단축키 |

확대 UI **6 통과**, 기존 UI **14 통과**, 파일 트리·스크롤·경로 복사 **11 통과**로 총 **31 통과 / 0 실패**였다. 전체 Rust 테스트는 **152 통과 / 0 실패 / 11 제외**였다. 초기 실행에서 입력 내용이 달라진 검색과 예전 복사 팝업을 기다리던 검사가 실패한 이력 및 최종 재실행 결과는 검증 요약에 함께 기록했다.

실행 결과와 소스·캡처·패키지 해시는 배율 검증 요약에 기록한다. 150% 화면, 125% 밝은 테마와 재실행, 200% 작은 창의 설정을 확인했다. 공간이 부족한 설정 항목명은 말줄임 처리하고 값은 별도 공간을 유지한다.

검사는 마커가 있는 격리 fixture와 별도 설정 파일을 사용한다. Windows 네이티브 창의 배율 동작은 이번에 실행하지 않았으며, Control 입력은 공통 단축키 단위 테스트와 macOS의 실제 키 입력으로 검사한다.

### 배율 조절 팝오버

타이틀바의 파일 트리 버튼 바로 왼쪽에 Lucide ZoomIn/ZoomOut 아이콘만 표시한다. 현재 배율 숫자는 팝오버에만 표시한다. 100%로 복원해도 팝오버가 열려 있으면 아이콘을 유지하고, 팝오버가 닫힐 때 함께 숨긴다. 버튼을 누르면 현재 배율·축소·확대·Reset이 있는 팝오버가 열린다. 아이콘과 팝오버는 배율과 무관하게 고정 크기이며 어두운·밝은 테마의 기존 색상 토큰을 사용한다.

단축키로 열린 팝오버는 마지막 변경 3초 후 닫힌다. 포인터가 팝오버 안에 있으면 유지하고, 벗어나면 다시 3초를 센다. 아이콘으로 직접 열면 외부 클릭·Escape·창 비활성화 시 닫는다. 검색창·파일명 편집의 포커스를 보존하며, 직접 연 팝오버에서는 Tab/Shift+Tab과 좌우 방향키로 버튼을 고르고 Enter/Space로 실행할 수 있다. Escape는 팝오버를 먼저 닫고 다음 입력에서 기존 편집·설정 화면에 적용된다.

```sh
python3 scripts/run_ui_zoom_tests.py --fixture /tmp/everything-file-tree-validation \
  --output .artifacts/zoom-icon-refinement-native
python3 scripts/run_gpui_ui_tests.py --fixture /tmp/everything-file-tree-validation \
  --output .artifacts/zoom-popover-regression
```

| TC | 조작과 확인 |
| --- | --- |
| ZOOM-07 | 아이콘 열기·닫기, 실제 마우스 축소·확대·초기화, 100%에서 아이콘을 유지하다 팝오버와 함께 닫기, 검색어·선택·창 위치 유지, 외부 클릭 전달 |
| ZOOM-08 | 반복 단축키 시 타이머 갱신, 100% 복원 후 3초가 지나면 팝오버와 아이콘 함께 숨김, hover 중 유지와 벗어난 뒤 닫힘 |
| ZOOM-09 | Tab/Shift+Tab·좌우 방향키·Enter/Space·Escape, 검색 문자열 보존 |
| ZOOM-10 | 밝은·어두운 테마, 400×300 창의 50~200% 배율, 고정 크기와 경계 버튼 비활성화, 인접 트리 토글 |

ZOOM-03과 ZOOM-06은 파일명 편집·설정 화면과 팝오버를 함께 열어 버튼 조작 및 Escape 우선순위도 확인한다. 최초 팝오버 구현에서는 배율 UI **10 통과**, 기존 UI 회귀 **14 통과**로 총 **24 통과 / 0 실패**였고, 전체 Rust 테스트는 **153 통과 / 0 실패 / 11 제외**였다. 이 실행 기록은 검증 요약의 `zoomPopover`에 있다.

`0.6.3`에서는 타이틀바의 숫자를 제거하고, 100% 복원 후 팝오버가 닫힐 때 아이콘도 함께 숨기도록 수정했다. 배율 UI **10 통과 / 0 실패**로 Reset 버튼, Command/Control 단축키, 자동 닫힘·외부 클릭·아이콘 클릭 및 편집·설정 화면을 검증했다. 결과와 소스·패키지 해시는 같은 검증 요약의 `zoomIconRefinement`에 기록했다. UI 검사 후 버전 선언과 lockfile만 0.6.3으로 올려 배포 패키지를 빌드했다.

`scripts/build_dmg.sh`로 만든 macOS arm64 `Everything_0.6.3_aarch64.dmg`는 Developer ID 서명, Apple 공증 승인·티켓 첨부, Gatekeeper 검사 및 DMG 무결성 검사를 통과했다. 앱 버전·번들 ID와 내장 LaunchAgent의 실행 경로도 확인했다. 최종 패키지 해시와 공증 제출 ID는 `zoomIconRefinement.productionRelease`에 기록했다.

## macOS 컨텍스트 메뉴

macOS 메뉴는 muda/AppKit으로 그린다. Open, 선택 시 Show Package Contents, Quick Look, Open With..., 구분선, Reveal in Finder, 구분선, Copy, Copy Path, 구분선, Move to Trash, 단일 선택 시 Rename 순서를 유지한다. 메뉴는 별도 네이티브 창이므로 본창 캡처에는 포함되지 않는다. 실제 메뉴 키보드 선택은 UI-07에서 검증했다.

## Rust 및 검색 회귀

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  cargo test --release --locked --features ui-automation \
  --manifest-path app/Cargo.toml -j4 -- --test-threads=1
```

**136 통과 / 0 실패**. HOME 스캔/운영 DB benchmark 2개, Swift 툴체인을 실행하는 아이콘 통합 테스트 1개, 네이티브 휴지통 통합 테스트 1개와 기존 수동 benchmark 7개는 기본 실행에서 제외한다. 파서·검색·DB·MCP·선택/페이지/삭제 상태와 설정 변경 알림·인덱싱 진행 상태 테스트가 포함된다.

초기 GPUI 전환 검증 중 기존 CPU QoS 타이밍 테스트가 한 번 실패했다(300ms wall time 동안 background CPU 0.080s 관측, 기대값 ≥0.15s). 해당 테스트 개별 실행과 전체 재실행은 코드나 기대값 변경 없이 통과했다. 초기 실행 요약에 이 이력도 기록했다.

후속 병합에서는 `npm test -- --release --features ui-automation -j4`와 전체 Rust 테스트를 다시 실행했다. 추가된 네이티브 휴지통 테스트도 명시적으로 실행해 **1 통과**를 확인했다. 고유한 이름의 임시 파일 여러 개와 하위 파일이 있는 폴더를 휴지통으로 옮긴 뒤 복원·정리한다. 첫 UI 실행에서는 UI-01의 시작 직후 문자 입력이 한 번 실패했고, 포커스 구현 변경 없이 독립 실행 6회에서 재현되지 않았다. 이후 전체 UI suite를 재실행했다. 병합 검증 기록에 결과와 이력을 기록했다.

Cmd+Delete 즉시 휴지통 이동 변경 후에도 `npm test`, 전체 Rust 테스트 **136 통과**와 UI suite **14 통과**를 확인했다. UI-09는 두 Command 단축키의 단일·다중 항목 이동, 확인창 부재와 삭제 결과 억제를 검사한다. 검색창과 이름 편집에서는 Command 단축키가 텍스트만 삭제하는지도 확인했다.

## macOS 배포 패키지

macOS 파일 작업·이름 변경 병합과 Cmd+Delete 즉시 휴지통 이동을 반영한 `0.6.0` 소스를 기본 기능의 production release로 빌드해 `.app`과 arm64 DMG를 다시 만들었다. `ui-automation` 기능은 배포 패키지에 포함하지 않는다.

```sh
python3 scripts/bundle_macos.py --dmg
```

번들 ID/버전/실행 파일과 내장 LaunchAgent 경로, 글꼴 라이선스, ad-hoc runtime 서명의 `codesign --verify --deep --strict`, WebKit 직접 링크 부재, CLI help/version을 확인했다. DMG checksum과 읽기 전용 마운트에서 실행 파일 SHA-256 및 Applications 링크도 확인했다. 패키지 안의 production 실행 파일을 fixture로 실행해 첫 프레임·500행/총 500,018개·오류 없음을 확인했고, 자동화 socket은 생성되지 않았다. 패키지 검증 기록에 해시와 결과가 있다. 앱 설치나 운영 데몬 등록은 수행하지 않았다.

파일 트리·스크롤 수정은 `0.6.1`의 production `.app`과 arm64 DMG에도 반영했다. 버전, 번들 ID, 내장 LaunchAgent·라이선스, 서명, DMG 무결성을 확인했으며 최신 패키지 해시는 파일 트리 검증 요약의 `scrollingFix.productionRelease`에 기록했다.

화면 배율을 포함한 `0.6.2`는 `scripts/build_dmg.sh`로 기본 기능의 production release를 다시 빌드했다. 로컬 설정에서 Developer ID 인증서를 자동으로 선택해 서명했으며, Apple 공증 `Accepted`, DMG 티켓 부착·검증, Gatekeeper `Notarized Developer ID`, `codesign --verify --deep --strict`와 DMG 무결성 검사를 통과했다. 번들 ID·버전·arm64 실행 파일과 내장 LaunchAgent도 확인했다. 배율 검증 요약의 `productionRelease`에 최종 패키지 해시를 기록했다.

Lucide 배율 팝오버와 100%에서 버튼 숨김을 반영한 최신 `0.6.2` 패키지도 같은 스크립트로 빌드·서명·공증했으며 Gatekeeper와 DMG 무결성 검사를 통과했다. 이 패키지의 해시와 공증 ID는 같은 검증 요약의 `zoomPopover.productionRelease`에 기록했다.

`0.6.3`의 DMG 자동 복사·메타데이터 갱신은 `scripts/build_dmg.sh`를 실제 실행해 확인했다. 워크트리와 기본 체크아웃의 `releases/`에 동일한 DMG·`latest-mac.yml`을 생성했고 최종 파일의 SHA-512·크기·버전과 YAML이 일치했다. Apple 공증·티켓 검증, Gatekeeper와 DMG 무결성 검사를 통과했으며, 격리된 Git 워크트리 회귀 검사를 포함한 패키징 테스트도 **11 통과 / 0 실패**였다. 기본 체크아웃의 다른 수정과 스테이징 상태는 보존했다. 실제 실행 결과와 최신 패키지 해시는 같은 검증 요약의 `worktreeReleasePublication`에 기록했다.

## Windows 빌드 확인의 한계

2026-11-14에 Windows 네이티브 빌드와 실제 창 검증을 추가로 완료했다. 최신 결과와 Windows 삭제 키 동작은 [Windows 검증 보고서](gpui-windows-validation.md)를 참고한다. 아래 내용은 이전 macOS 호스트에서 시도했던 교차 빌드 기록이다.

이 Mac에 설치된 Rust `x86_64-pc-windows-msvc` target으로 다음 교차 검사를 시도했다.

```sh
cargo check --release --locked --target x86_64-pc-windows-msvc \
  --manifest-path app/Cargo.toml -j4
```

`ring 0.17.14`의 C 의존성 컴파일에서 Windows용 `assert.h`를 찾지 못해 exit 101로 중단됐다. 이 호스트에는 Windows C toolchain/SDK가 없으므로 앱의 Windows Rust 타입 검사까지 완료하지 못했다. 실행 요약에 원인을 기록했다. 제공한 `windows-latest` workflow의 native check/test 및 NSIS build는 이번 로컬 작업에서 실행하지 않았으며, Windows 빌드·설치·실행 성공으로 주장하지 않는다.

## 검증 범위

Windows 실행, Explorer 네이티브 메뉴, 실제 IME 조합, Finder 등 외부 앱으로 드래그, 시스템 글로벌 단축키 충돌, 실제 FDA 변경/데몬 재등록은 이 macOS fixture 테스트에서 검증하지 않았다. Windows build/NSIS workflow는 별도로 제공한다. 운영 앱과 데몬을 종료하거나 운영 인덱스를 초기화하지 않았다.
