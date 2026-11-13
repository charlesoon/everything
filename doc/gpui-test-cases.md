# GPUI 전환 테스트

화면 기준은 Tauri 커밋 `68e4ddf`와 그 소스로 실행한 실제 창이다. UI를 새로 디자인하지 않고 글꼴, 색상, 배치와 메뉴를 유지한다. 이후 `macos-daemon-writer`의 `b2f2686`, `86f525b`를 병합하면서 macOS 이름 변경 제스처도 GPUI에 반영했다. macOS 26.7.3 / Apple M5 Pro에서 release 바이너리를 테스트했다.

## 자동화 방법

`ui-automation` 기능으로 빌드한 앱에 0600 Unix socket을 명시적으로 전달한다. 키·마우스는 AppKit NSEvent로, 확정 문자는 현재 키보드 언어에 영향을 받지 않는 NSTextInputClient `insertText:replacementRange:`로 전달해 GPUI의 실제 입력·포커스·hit testing을 거친다. UI 상태 JSON은 관측용이며 상태를 바꾸는 명령은 없다. 파일 작업은 마커가 있는 전용 fixture 안에서만 수행하고 원래 파일을 복원한다. FDA, 로그인 항목, 실제 데몬 등록과 Reset은 fixture 모드에서 실행하지 않는다.

```sh
python3 scripts/ui_fixture.py /tmp/everything-ui-fixture --entries 500000
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  cargo build --release --locked --features ui-automation --manifest-path src-tauri/Cargo.toml
python3 scripts/run_gpui_ui_tests.py --fixture /tmp/everything-ui-fixture \
  --output doc/gpui-evidence/ui
```

실행 결과: **14 통과 / 0 실패**. 기계 판독 결과와 실행 시각은 [ui-results.json](gpui-evidence/ui/ui-results.json)에 있다.

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
| UI-09 | 휴지통 기본 Cancel, Tab+Enter 확인, 실제 삭제와 pending-trash 결과 억제 | 통과 |
| UI-10 | 앱에만 적용한 시스템 light/dark, fixture Reset 차단, 400×300 창과 확인창 | 통과 |
| UI-11 | Enter/Cmd+O 열기, Cmd+Enter 표시, Space Quick Look의 OS 요청 완료 | 통과 |
| UI-12 | 컬럼/창 크기 저장, 두 번 재시작 후 크기 증가 없음 | 통과 |
| UI-13 | 온보딩 키보드 포커스 유지, 나중에 선택, FDA 설정 버튼의 안내 변경과 배너 닫기 | 통과 |
| UI-14 | 시스템 더블클릭 간격 후 Rename, macOS F2 비활성, Escape·더블클릭 Open·입력창 포커스·창 비활성화 시 예약 취소 | 통과 |

UI-11은 요청 완료를 검증한다. 외부 편집기나 Quick Look 내용의 화면 검증으로 해석하지 않는다. 한글 TC는 확정 문자 입력이며 실제 한국어 IME 조합 중간 상태 검증은 아니다.

## Tauri 화면 비교

동일 fixture, 900×560 logical pixels, 동일 이벤트 순서로 원본과 GPUI를 각각 캡처했다. Inter 400/500/600, JetBrains Mono 400, 원본 SVG와 네이티브 파일 아이콘을 사용한다. 제목 30px, 검색창 34px, 헤더 33px, 행 27px, 상태바 33px 및 컬럼 최소 너비를 원본 CSS에서 가져왔다. 테마는 시스템을 따른다.

- [어두운 테마: 왼쪽 Tauri / 오른쪽 GPUI](gpui-evidence/visual/comparison-dark.png)
- [밝은 테마: 왼쪽 Tauri / 오른쪽 GPUI](gpui-evidence/visual/comparison-light.png)
- [전체 캡처 목록](gpui-evidence/visual/captures.json): 두 테마 × 초기/검색/선택/이름 변경/휴지통/결과 없음 = 각 렌더러 12장.
- [채널별 평균 절대 픽셀 차이](gpui-evidence/visual/pixel-comparison.json): 0–255 척도의 참고 지표. 유사도 백분율이나 완전한 픽셀 일치를 뜻하지 않는다. 렌더러의 글자 안티앨리어싱, caret/선택 표시와 OS 포커스 링에 미세한 차이가 남는다.

macOS 메뉴는 Tauri가 사용하던 동일 버전 muda/AppKit으로 그린다. Open, 선택 시 Show Package Contents, Quick Look, Open With..., 구분선, Reveal in Finder, 구분선, Copy, Copy Path, 구분선, Move to Trash, 단일 선택 시 Rename 순서를 유지한다. 메뉴는 별도 네이티브 창이므로 본창 캡처에는 포함되지 않는다. 실제 메뉴 키보드 선택은 UI-07에서 검증했다.

## Rust 및 검색 회귀

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  cargo test --release --locked --features ui-automation \
  --manifest-path src-tauri/Cargo.toml -j4 -- --test-threads=1
```

**136 통과 / 0 실패**. HOME 스캔/운영 DB benchmark 2개, Swift 툴체인을 실행하는 아이콘 통합 테스트 1개, 네이티브 휴지통 통합 테스트 1개와 기존 수동 benchmark 7개는 기본 실행에서 제외한다. 파서·검색·DB·MCP·선택/페이지/삭제 상태와 설정 변경 알림·인덱싱 진행 상태 테스트가 포함된다.

초기 GPUI 전환 검증 중 기존 CPU QoS 타이밍 테스트가 한 번 실패했다(300ms wall time 동안 background CPU 0.080s 관측, 기대값 ≥0.15s). 해당 테스트 개별 실행과 전체 재실행은 코드나 기대값 변경 없이 통과했다. [초기 실행 요약](gpui-evidence/rust-results.json)에 이 이력도 기록했다.

후속 병합에서는 `npm test -- --release --features ui-automation -j4`와 전체 Rust 테스트를 다시 실행했다. 추가된 네이티브 휴지통 테스트도 명시적으로 실행해 **1 통과**를 확인했다. 고유한 이름의 임시 파일 여러 개와 하위 파일이 있는 폴더를 휴지통으로 옮긴 뒤 복원·정리한다. 첫 UI 실행에서는 UI-01의 시작 직후 문자 입력이 한 번 실패했고, 포커스 구현 변경 없이 독립 실행 6회에서 재현되지 않았다. 이후 전체 UI suite를 재실행했다. [병합 검증 기록](gpui-evidence/merge-results.json)에 결과와 이력을 기록했다.

Tauri/GPUI의 실제 production search engine에 동일한 36개 케이스를 넣고 반환 경로 전체, 결과 수와 mode를 비교했다. **36개 모두 일치**한다. 케이스당 warmup 3회 후 10회 측정했다. [측정 보고서](gpui-performance.md)와 [원시 검색 결과](gpui-evidence/performance/gpui-engine.json)를 참고한다.

## macOS 배포 패키지

병합 전 `0.6.0` 소스를 기본 기능의 production release로 빌드해 `.app`과 arm64 DMG를 만들었다. `ui-automation`을 넣은 성능 측정 바이너리와 구분한다. 후속 병합의 검증 결과는 위 병합 검증 기록에 별도로 남겼다.

```sh
python3 scripts/bundle_macos.py --dmg
```

번들 ID/버전/실행 파일과 내장 LaunchAgent 경로, 글꼴 라이선스, ad-hoc runtime 서명의 `codesign --verify --deep --strict`, WebKit 직접 링크 부재, CLI help/version을 확인했다. DMG checksum과 읽기 전용 마운트에서 실행 파일 SHA-256 및 Applications 링크도 확인했다. 패키지 안의 production 실행 파일을 fixture로 실행해 첫 프레임·500행/총 500,018개·오류 없음을 확인했고, 자동화 socket은 생성되지 않았다. [패키지 검증 기록](gpui-evidence/package-results.json)에 해시와 결과가 있다. 앱 설치나 운영 데몬 등록은 수행하지 않았다.

## Windows 빌드 확인의 한계

이 Mac에 설치된 Rust `x86_64-pc-windows-msvc` target으로 다음 교차 검사를 시도했다.

```sh
cargo check --release --locked --target x86_64-pc-windows-msvc \
  --manifest-path src-tauri/Cargo.toml -j4
```

`ring 0.17.14`의 C 의존성 컴파일에서 Windows용 `assert.h`를 찾지 못해 exit 101로 중단됐다. 이 호스트에는 Windows C toolchain/SDK가 없으므로 앱의 Windows Rust 타입 검사까지 완료하지 못했다. [실행 요약](gpui-evidence/windows-check.json)에 원인을 기록했다. 제공한 `windows-latest` workflow의 native check/test 및 NSIS build는 이번 로컬 작업에서 실행하지 않았으며, Windows 빌드·설치·실행 성공으로 주장하지 않는다.

## 검증 범위

Windows 실행, Explorer 네이티브 메뉴, 실제 IME 조합, Finder 등 외부 앱으로 드래그, 시스템 글로벌 단축키 충돌, 실제 FDA 변경/데몬 재등록은 이 macOS fixture 테스트에서 검증하지 않았다. Windows build/NSIS workflow는 별도로 제공한다. 운영 앱과 데몬을 종료하거나 운영 인덱스를 초기화하지 않았다.
