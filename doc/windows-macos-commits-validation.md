# macOS 변경의 Windows 검증 — 0.6.3

2026-11-14, `gpui-ui`의 `85dc8db`와 이번 Windows 수정 사항을 검증했다.
환경은 Windows 11 build 26300 x64, 디스플레이 배율 150%, Rust 1.97.1이다.
실제 GPUI 창 테스트 **31 통과 / 0 실패**, Rust 테스트 **143 통과 / 0 실패**,
설치 전 종료·백업 검사 **18 통과 / 0 실패**다. 설치파일도 로컬에서 생성했다.

## 대상 커밋

이전 Windows 검증 기준인 `8938ba8` 이후의 커밋을 확인했다.

| 커밋 | 변경 | Windows 확인 |
| --- | --- | --- |
| `6347627` | 상태 표시줄 자동 시작 토글 제거 | 새 상태 표시줄과 설정 진입 |
| `48b37ea` | 파일 트리, 경로 복사 | NEW-02–07, WIN-07–08 |
| `a68f311` | zinc 색상, Lucide 파일 아이콘 | 실제 dark/light 렌더링 |
| `6c657de` | 설정 팝오버, Geist, 입력·아이콘 | NEW-01, NEW-12–13 |
| `7d1898b` | 0.6.2 버전 | 후속 0.6.3과 버전 일치 확인 |
| `ee76763` | 새 타이틀바, 중간 생략 경로, UI 확대 | NEW-05–14, WIN-16–17 |
| `0c21adf` | 로컬 macOS 서명 설정 | Windows 빌드 경로에서 호출하지 않음 |
| `32af1d5` | 확대 조작 개선, 0.6.3 | NEW-08–14, 0.6.3 패키징 |
| `85dc8db` | DMG·macOS 업데이트 메타데이터 | Windows 실행·NSIS 패키징과 분리됨 |

## 실제 창에서 확인한 동작

500,018개 항목의 격리된 인덱스를 사용했다. 새 기능 검사는 여기에 실제 파일
160개, Windows 숨김 속성 파일, 인덱스에 없는 파일, 24단계 폴더를 추가했다가
검사 후 원복한다. 두 UI suite는 전역 마우스·키보드를 공유하므로 순서대로 실행했다.

| 범위 | 검증한 동작 | 결과 |
| --- | --- | --- |
| 파일 트리 | 선택 결과 따라가기, 모든 형제·숨김·미인덱싱 항목, 가상 목록 스크롤, 24단계 탐색, 없는 경로 오류와 복구 | NEW-02–04, NEW-07 통과 |
| 트리 상태 | 새로고침·최소화 후 스크롤, 상위 폴더, 패널 크기 변경, 트리 포커스에서 Delete/Shift+Delete 차단 | NEW-04–06 통과 |
| 경로 | `~` 축약과 중간 생략 표시, 푸터·트리 경로 클릭 시 전체 한글 경로 복사 | NEW-05 통과 |
| 설정·디자인 | Windows 설정 항목과 키보드 선택, dark/light의 Geist·Lucide, 400×300 창의 설정 버튼 | NEW-01, NEW-12–13 통과 |
| 확대 | Ctrl+=/Ctrl+Shift++/Ctrl+-/Ctrl+0, 일반 +/- 입력, 50–200% 제한, 마우스·키보드 조작, 자동 닫힘·호버 유지, 재시작 복원 | NEW-08–14 통과 |
| 선택·이름 변경 | 확대 중 선택·트리 스크롤·F2 이름 변경 유지, 충돌·잘못된 이름 오류 | NEW-08–09, WIN-09 통과 |
| 검색·표 | 한글·공백·glob·Windows 경로, 빠른 검색 교체, 1,000행 이상 로딩, 정렬·컬럼 크기·다중 선택·클립보드·Explorer 메뉴 | WIN-01–08 통과 |
| 파일 동작 | Enter/Ctrl+O/더블클릭 열기, Explorer 표시, Delete 확인 없이 휴지통, Shift+Delete 확인 없이 영구 삭제, 폴더·하위 항목 중복 처리와 이웃 보존 | WIN-10, WIN-13–15 통과 |
| 창·환경설정 | 최소 크기, 재시작 후 창·컬럼 복원, 최소화·최대화·복원·닫기, 제목 글자·빈 공간 드래그, 최대화 상태에서 끌어 복원 | WIN-11–12, WIN-16–17 통과 |

## 수정 사항

- 홈 경로 축약이 `/` 구분자만 인식하던 문제를 `Path::strip_prefix`로 수정했다.
  Windows의 `~\folder`가 표시되며, 이름이 비슷한 이웃 경로를 축약하지 않는
  네이티브 경로 단위 테스트를 추가했다.
- 400×300 창에서 UI를 200%로 확대하면 트리 크기 조절 구분선이 타이틀바의
  트리 버튼과 겹쳤다. 버튼을 위에 배치해 클릭이 전달되도록 수정하고 실제 클릭으로 확인했다.
- 자동화는 새 타이틀바·설정 메뉴 위치를 사용하고, 렌더링된 목록의 항목 수와
  콘텐츠 높이를 함께 관측한다. GPUI의 목록 viewport 높이를 행 높이로 쓰거나
  이전 프레임의 높이를 새 검색 결과 개수로 나누지 않도록 했다.
- Win32 입력은 테스트 HWND 활성화를 확인한 뒤 전송한다. fixture 전용 테마
  환경 변수는 `ui-automation` 빌드에서만 적용하며 Windows 설정을 변경하지 않는다.

## 테스트와 패키징

| 검사 | 결과 |
| --- | --- |
| `cargo test --release --locked --features ui-automation --manifest-path app/Cargo.toml -- --test-threads=1` | 143 통과, 수동 스트레스 벤치마크 8개 제외 |
| `npm test -- --release --features ui-automation` | 통과 |
| `scripts/run_windows_ui_feature_tests.py` | 14 통과 |
| `scripts/run_gpui_windows_tests.py` | 17 통과 |
| `scripts/test-windows-installer.ps1` | x64 호스트 9개, x86 호스트 9개 통과 |
| `scripts\build-windows.ps1` | 기본 기능의 x64 release 앱 및 NSIS 설치파일 생성 성공 |

설치 검사는 서비스 상태를 주입한 모델로 정상 종료, 중단 실패 시 설치 차단과
설정 복구, 종료 후 DB/WAL/SHM의 바이트 단위 백업을 확인한다. 별도로 소유한
x64 테스트 프로세스를 실제 종료해 PID·실행 경로 검증도 확인했다.

설치파일: [Everything_0.6.3_x64-setup.exe](../app/target/release/bundle/nsis/Everything_0.6.3_x64-setup.exe)

- 크기: 5,633,812 bytes
- SHA-256: `94fbb0e60ec50fc867e2b9218def20aa5e5e1672fa53d3f54b26e7407e0fa228`
- [실행 파일·설치파일 해시와 검증 수치](../.artifacts/windows-macos-commits/build-manifest.json)
- [새 기능 UI 결과](../.artifacts/windows-macos-commits/features-final/feature-results.json), [기존 Windows UI 결과](../.artifacts/windows-macos-commits/regression-final/ui-results.json)
- [Rust 테스트·UI 빌드 로그](../.artifacts/windows-macos-commits/ui-build-final.log), [npm test 로그](../.artifacts/windows-macos-commits/npm-test-final.log)
- [설치 종료·백업 검사 로그](../.artifacts/windows-macos-commits/installer-tests-final.log), [로컬 Windows 빌드 로그](../.artifacts/windows-macos-commits/windows-build-final.log)
- 화면: [dark 트리](../.artifacts/windows-macos-commits/features-final/02-tree-dark.png), [light·확대·설정](../.artifacts/windows-macos-commits/features-final/09-light-settings-zoom.png), [작은 창·200%](../.artifacts/windows-macos-commits/features-final/05-small-zoom-200-dark.png)

## 재현

Windows 대화형 데스크톱과 Python Pillow가 필요하다. fixture는 새 경로에 만든다.

```powershell
cargo test --release --locked --features ui-automation --manifest-path app/Cargo.toml -- --test-threads=1
cargo build --release --locked --features ui-automation --manifest-path app/Cargo.toml
python -X utf8 scripts/ui_fixture.py .artifacts/windows-macos-repro/fixture --entries 500000
Copy-Item app/target/release/everything.exe .artifacts/windows-macos-repro/everything-ui-automation.exe -Force
python -X utf8 scripts/run_windows_ui_feature_tests.py --binary .artifacts/windows-macos-repro/everything-ui-automation.exe --fixture .artifacts/windows-macos-repro/fixture --output .artifacts/windows-macos-repro/features
python -X utf8 scripts/run_gpui_windows_tests.py --binary .artifacts/windows-macos-repro/everything-ui-automation.exe --fixture .artifacts/windows-macos-repro/fixture --output .artifacts/windows-macos-repro/regression
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-windows-installer.ps1
$env:Path = (Join-Path $env:LOCALAPPDATA 'tauri\NSIS') + ';' + $env:Path
& .\scripts\build-windows.ps1
```

운영 서비스·DB·사용자 설정은 변경하지 않았다. 실제 설치파일을 실행하는 운영
업그레이드, 운영 인덱스의 장시간 갱신, IME 조합 중간 상태, 외부 앱의 표시 내용과
외부 앱으로 드래그는 이번 검증 범위에 포함하지 않는다. 한글 입력은 확정 문자로
검사했다. 원시 로그·스크린샷·실행 파일은 로컬 `.artifacts/`와 `target/`에 보관한다.
