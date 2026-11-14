# GPUI Windows 검증

2026-11-14, `gpui-ui`의 `e70ecd9d68aa84cf3744d991866fb720e8f38f1f`와 현재 작업 트리를 Windows에서 빌드하고 실행했다. Rust 단위 테스트 **126 통과 / 0 실패**, 실제 창 테스트 **16 통과 / 0 실패**다. 후반에 추가된 Windows Temp 문서 테스트도 포함했다. 수동 스트레스 벤치마크 8개는 기본 설정대로 제외했다.

## 환경과 방법

- Windows 11 x64, 26H2 / build 26300.9818
- Rust 1.97.1, MSVC / Windows SDK
- 사용 가능한 GPU: NVIDIA GeForce RTX 4060, AMD Radeon Graphics
- 500,018행의 별도 SQLite v7 fixture, 실제 파일 14개와 디렉터리 4개
- Win32 `SendInput`으로 실제 GPUI 창의 키보드·마우스를 조작하고, 읽기 전용 UI 상태와 창 캡처로 관측했다. UI 모델을 직접 변경하지 않았다.
- 실제 파일 작업은 `.everything-ui-fixture` 마커가 있는 디렉터리 안에서만 실행했다. 운영 인덱스, 서비스 등록과 사용자 설정은 변경하지 않았다.

## 확인한 동작

| 항목 | 실제 확인 | 결과 |
| --- | --- | --- |
| 시작·검색 | 첫 화면, 검색 포커스, 한글·악센트 문자·공백, 대소문자, glob, Windows 절대/상대 경로, 입력 삭제 | WIN-01–03 통과 |
| 결과 표 | PageDown으로 1,000행 이상 로딩, 서버 정렬, 컬럼 드래그, Ctrl/Shift 다중 선택 | WIN-04–06 통과 |
| 클립보드·Explorer 메뉴 | Ctrl+C 및 네이티브 Copy Path의 실제 클립보드 내용이 한글 경로와 정확히 일치 | WIN-07–08 통과 |
| 이름 변경 | F2, 확장자를 남긴 이름 선택, 성공, 이름 충돌·잘못된 이름의 오류 처리 | WIN-09 통과 |
| Windows Delete | 확인창 없이 휴지통 이동, 삭제 결과 숨김 | WIN-10 통과 |
| 크기·설정 | 400×300 화면, fixture Reset 차단, 두 번 재시작 후 창·컬럼 크기 유지 | WIN-11–12 통과 |
| `e70ecd9` 병합 동작 | Windows F2 유지, 선택 항목 재클릭 시 macOS 지연 이름 변경이 발생하지 않음, Enter/Ctrl+O 열기, Ctrl+Enter 표시, 더블클릭 열기, 검색 입력의 일반 복사 | WIN-13 통과 |
| 여러 항목 휴지통 이동 | 폴더와 그 하위 파일을 함께 선택해 이동, 휴지통 안의 실제 내용 확인, 이름이 비슷한 이웃 폴더 보존 | WIN-14 통과 |
| Windows Shift+Delete | 확인창 없이 파일·폴더 영구 삭제, 휴지통에 새 항목이 생기지 않음, 검색 입력 포커스에서는 파일을 삭제하지 않음 | WIN-15 통과 |
| 창 버튼 | 아이콘 표시, 최대화·복원·최소화·닫기의 실제 HWND 상태 | WIN-16 통과 |

Enter/Open/Reveal은 앱의 OS 요청 완료를 확인했다. 외부 편집기의 내용 표시까지 검증한 것은 아니다. 한글 입력은 확정 문자 입력이며 IME 조합 중간 상태는 포함하지 않는다.

## 수정 사항

- Windows의 UTF-8 `clip` 파이프에서 한글 경로가 깨졌다. Ctrl+C와 Explorer Copy Path가 공통 Win32 `CF_UNICODETEXT` 구현을 사용하도록 바꿨다. BOM이나 추가 문자가 붙지 않는지도 비교했다.
- Windows 창 버튼 SVG에 색상이 지정되지 않아 아이콘이 보이지 않았다. 명시적인 색상을 지정했다.
- GPUI 0.2.2의 Windows `zoom()`은 최대화만 수행했다. 최대화 버튼과 제목 표시줄 더블클릭이 Win32 최대화/복원을 전환하도록 보완했다.
- 최신 요청에 따라 Windows 키 동작을 `Delete = 확인 없이 휴지통`, `Shift+Delete = 확인 없이 영구 삭제`로 구현했다. 원격 `dcc0b8b`의 macOS Command 단축키 즉시 휴지통 이동과 일반 Delete 확인 동작도 함께 유지한다.
- 영구 삭제는 기존 다중 선택·하위 항목 중복 제거·항목별 실패 처리를 공유한다. 실제 잠긴 파일의 실패가 다른 파일 삭제를 막지 않는 Rust 테스트도 추가했다.
- 기존 테스트의 Unix 경로 가정과 fixture의 Windows verbatim 경로 차이를 수정했다. 인덱스 형식은 유지한다. 별도의 Temp 수정은 Windows 기본 인덱싱 범위를 사용자 Temp 문서로 확장하며, 명시적인 `.pathignore` 제외 규칙은 계속 적용한다.

## 재현

Windows의 대화형 데스크톱과 Python Pillow가 필요하다. fixture 경로는 새 디렉터리를 사용한다.

```powershell
cargo test --release --locked --manifest-path src-tauri/Cargo.toml -- --test-threads=1
cargo build --release --locked --features ui-automation --manifest-path src-tauri/Cargo.toml
python -X utf8 scripts/ui_fixture.py .artifacts/windows-gpui/new-fixture --entries 500000
python -X utf8 scripts/run_gpui_windows_tests.py --fixture .artifacts/windows-gpui/new-fixture --output .artifacts/windows-gpui/new-ui
```

Windows에서는 테스트 드라이버가 HWND와 `SendInput`을 사용한다. `ui-automation` 기능의 AppKit/Unix socket 드라이버는 macOS 전용이다. Windows에서도 fixture 격리와 읽기 전용 상태 관측을 사용한다.

로컬 원시 자료는 생성물로 유지하며 커밋하지 않는다.

- [최종 UI 결과와 실행 파일 SHA-256](../.artifacts/windows-gpui/precommit-ui/ui-results.json)
- [커밋 전 npm test 및 전체 Rust 테스트 로그](../.artifacts/windows-gpui/precommit-tests.log)
- [Windows 빌드 로그](../.artifacts/windows-gpui/build-ui-automation.log)
- [한글 검색 화면](../.artifacts/windows-gpui/precommit-ui/02-korean.png)
- [창 버튼 화면](../.artifacts/windows-gpui/precommit-ui/13-window-controls.png)

설치·업그레이드·서비스 재등록, 운영 인덱스의 실시간 갱신, 실제 IME 조합과 외부 앱으로 드래그는 이번 fixture 검증 범위에 포함하지 않았다. NSIS 설치 파일은 만들지 않았다.

## 타이틀바 드래그 후속 검증

Windows에서 `start_window_move()`가 기본 no-op인 상태였고, 창 전체의
`track_focus`가 마우스 기본 동작도 차단하고 있었다. 창 버튼을 제외한
타이틀바를 `WindowControlArea::Drag`로 등록하고 Windows의 테이블 포커스
범위를 결과 영역으로 옮겼다. macOS의 기존 포커스·제목 표시줄 처리는 유지한다.

수정 전에는 제목 표시줄이 `HTCLIENT`였고 드래그 전후 창 좌표가 같았다.
수정 후에는 `HTCAPTION`이며, 150% 배율에서 빈 공간을 끌었을 때
`(139, 144) → (274, 219)`, 제목 글자를 끌었을 때
`(274, 219) → (184, 294)`로 실제 HWND 위치가 바뀌었다.

추가한 WIN-17은 두 드래그 영역, 창 크기 유지, 버튼·검색창의 드래그 영역
제외, 네이티브 더블클릭 최대화와 최대화 상태에서 끌어 복원을 확인한다.
전체 Windows UI suite **17 통과 / 0 실패**로 재검증했다.

초기 실행에서 일괄 Unicode 입력의 문자 누락과 빠른 포인터 이동 직후의
클릭 누락을 관측했다. 테스트 드라이버가 문자 패킷 사이에 5ms,
포인터 이동과 클릭 사이에 75ms를 두도록 조정한 뒤 전체 suite를 통과했다.

- [타이틀바 수정 후 전체 UI 결과](../.artifacts/windows-gpui/titlebar-verified-ui/ui-results.json)
- [타이틀바 드래그 캡처](../.artifacts/windows-gpui/titlebar-verified-ui/14-titlebar-drag.png)
