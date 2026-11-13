# Tauri → GPUI 성능 비교

같은 fixture와 release 빌드에서 검색 결과를 표시한 GUI의 RSS 중앙값은 **287.4 → 117.1MiB**, 프로세스 실행부터 첫 프레임 콜백 확인까지의 중앙값은 **381.9 → 171.5ms**였다. 입력부터 검색 결과 프레임 콜백까지는 **225.5 → 217.4ms**였다. 기존 200ms 검색 스케줄링과 UI를 유지한 결과다.

검색 엔진은 유지했다. 같은 36개 케이스의 반환 경로 전체·결과 수·검색 mode는 모두 일치했다. 이 측정은 한 머신과 합성 데이터의 결과이며, 운영 인덱스나 Windows의 성능을 대표하지 않는다.

## 환경과 비교 조건

| 항목 | 조건 |
| --- | --- |
| 측정 시작 | 2026-11-14 00:45:41 KST |
| 머신 | Apple M5 Pro, RAM 48GiB, arm64 |
| OS / Rust | macOS 26.7.3 (25G342) / rustc 1.97.1 |
| 기준 버전 | Tauri 커밋 `68e4ddf`, 시작 당시 로컬 변경까지 보존한 소스 |
| 변경 버전 | `gpui-ui` worktree의 GPUI 0.2.2 구현 |
| 빌드 | 두 버전 모두 release, 동일한 opt-in UI 이벤트 드라이버 |
| 데이터 | SQLite v7 / FTS5 trigram, 총 500,018개 엔트리(가상 500,000개와 실제 파일/폴더) |
| UI | 900×560 logical pixels, 같은 파일·아이콘·원본 글꼴·컬럼, 기본 500행 |
| 격리 | `EVERYTHING_DATA_DIR`를 fixture로 지정, 운영 DB·데몬 설정 변경 없음 |

측정 중 컴파일을 실행하지 않았다. 완전히 유휴 상태인 전용 머신은 아니며, 시작 시 시스템 load average는 2.92 / 4.09 / 4.73이었다. [환경 원시 기록](gpui-evidence/performance/environment.json)과 두 바이너리의 SHA-256을 [comparison.json](gpui-evidence/performance/comparison.json)에 보관했다.

## 시작과 검색 응답

| 지표 | 표본 수(버전별) | Tauri p50 / p95 | GPUI p50 / p95 |
| --- | --- | --- | --- |
| 프로세스 실행 → 첫 프레임 콜백 확인 | 7 | 381.9 / 394.8ms | 171.5 / 184.4ms |
| 앱 내부 시작 → 첫 프레임 콜백 | 7 | 368.0 / 382.0ms | 159.9 / 171.0ms |
| 입력 이벤트 → 검색 결과 프레임 콜백 | 60 | 225.5 / 246.0ms | 217.4 / 241.1ms |
| 드라이버 요청 → 콜백 확인 | 60 | 242.9 / 264.4ms | 236.2 / 257.1ms |

시작 측정은 두 앱의 실행 순서를 번갈아 7회씩 수행했다. 매번 프로세스를 새로 실행했지만 OS 파일 캐시를 비우지 않았으므로 cold boot 측정은 아니다. 검색은 `report`, `한글`, `Alpha`, `*.md`, `space name`, 결과 없음의 6종을 2회씩 예열한 뒤 10회씩 측정했다. 해당 입력의 첫 결과 프레임을 사용했으며, 이번 표본은 양쪽 모두 `includeTotal=true`였다.

양쪽 드라이버 모두 키 조작은 NSEvent로, 확정 문자는 실제 first responder의 NSTextInputClient `insertText:replacementRange:`로 전달한다. 입력기 언어에 따른 합성 키 해석을 피한 동일한 방법이며, 실제 IME 조합 과정의 지연을 측정한 것은 아니다.

Tauri는 DOM 갱신 후 두 번의 `requestAnimationFrame`과 Rust 기록 호출을, GPUI는 상태 반영 후 두 번의 `on_next_frame` 콜백을 사용했다. 둘 다 프레임 콜백 기반 관측이며 실제 디스플레이 scanout 시각이나 광학적인 입력 지연을 측정하지 않는다. 두 렌더러의 콜백 의미와 계측 오버헤드도 완전히 같지는 않다. 내부 입력 지연에는 기존 200ms leading/trailing 스케줄링이 포함되고, 드라이버 측 시간에는 socket·이벤트 전달·폴링도 포함된다.

| 쿼리 | Tauri 입력→프레임 p50 | GPUI 입력→프레임 p50 |
| --- | --- | --- |
| `report` | 245.5ms | 241.0ms |
| `한글` | 223.0ms | 217.8ms |
| `Alpha` | 218.0ms | 216.4ms |
| `*.md` | 236.5ms | 223.6ms |
| `space name` | 233.0ms | 215.2ms |
| 결과 없음 | 217.5ms | 215.4ms |

## 메모리와 유휴 CPU

아래는 버전별 **한 번의 실행**에서 각 상태를 여러 번 관측한 중앙값이다. 독립적인 메모리 trial 여러 회로 해석하지 않는다. 빈 입력도 원본과 동일하게 전체 인덱스의 첫 500행을 표시한다.

| 상태 | 표본 수 | Tauri RSS | GPUI RSS | Tauri physical footprint | GPUI physical footprint |
| --- | --- | --- | --- | --- | --- |
| 초기 빈 입력, 첫 500행 | 3 | 218.3MiB | 97.8MiB | 205.0MiB | 156.3MiB |
| 예열/측정 후 `report` 결과 500행 | 5 | 287.4MiB | 117.1MiB | 680.8MiB | 197.5MiB |
| 추가 120회 검색 후 같은 `report` | 5 | 296.3MiB | 117.1MiB | 1,383.1MiB | 197.5MiB |

RSS는 `ps`의 resident set 합계, physical footprint는 macOS `proc_pid_rusage`의 `ri_phys_footprint` 합계다. 공유 페이지·압축 메모리·그래픽 자원 등에 대한 회계가 달라 두 지표를 서로 바꾸어 사용하지 않는다. Tauri의 footprint 증가는 이 실행에서 관측한 값이며, 장기 메모리 누수의 증명으로 해석하지 않는다.

Tauri GUI에는 본체와 WebContent/GPU/Networking XPC 프로세스를 포함했다. launchd가 부모인 XPC는 격리 실행 중 새로 생긴 WebKit PID로 연결했고, 해당 3개 PID가 앱 종료 후 함께 사라진 것을 확인했다. `responsiblePid`는 실행을 시작한 공통 호스트를 가리키므로 이것만으로 앱 소유권을 판정하지 않았다. GPUI에서 관측한 GUI 프로세스는 본체 하나였다. PID·명령·종료 여부는 [comparison.json](gpui-evidence/performance/comparison.json)의 `associatedProcesses`에 있다.

결과 상태에서 2초간 CPU time 증가량을 측정한 유휴 CPU는 Tauri **3.71%**, GPUI **1.48%**였다(한 코어 100% 기준, 버전별 1구간). 짧은 표본이라 세밀한 차이의 일반화에는 적합하지 않다. 이 머신의 `proc_pid_rusage` user/system 값은 mach ticks이므로 `mach_timebase_info`의 125/3을 곱해 ns로 변환했다. 독립적인 200ms CPU burn에서 Python 프로세스 CPU 시간과 변환값의 비율은 1.000128이었다. [단위 확인 기록](gpui-evidence/performance/cpu-accounting-check.json)에 원시값을 보관했으며, 초기 보고서의 ticks를 ns로 취급한 CPU 수치는 폐기했다.

공용 운영 인덱서 PID 1879는 GUI 합계에서 제외했으며, 별도 표본 RSS는 **51.1MiB**, footprint는 **50.9MiB**였다. fixture 검색은 이 운영 인덱서를 사용하지 않는다.

## 검색 엔진 회귀와 시간

이름·대소문자·한글·공백·Unicode·확장자·glob·절대/상대/홈 경로·디렉토리 경계·결과 없음·각 컬럼 양방향 정렬·페이지·제한값을 포함하는 **36개 케이스 모두 일치**했다. 케이스별 3회 예열 후 10회 측정했다.

| 대표 케이스 | Tauri 엔진 p50 | GPUI 엔진 p50 |
| --- | --- | --- |
| 이름 `report` | 6.668ms | 6.708ms |
| 한글 | 0.022ms | 0.023ms |
| 확장자 `*.md` | 1.238ms | 1.287ms |
| 두 번째 페이지 | 0.583ms | 0.592ms |
| 경로 오름차순 | 87.386ms | 88.851ms |
| 수정 시각 오름차순 | 119.622ms | 129.355ms |

서로 다른 난이도의 전체 360개 표본을 합치면 엔진 p50/p95는 Tauri 0.673/87.386ms, GPUI 0.680/88.563ms다. 쿼리 구성에 의존하는 합계이므로 각 케이스를 함께 봐야 한다. 공통 엔진의 유의미한 속도 개선을 주장하지 않으며, 일부 전체 정렬은 기존 backend 30ms 목표도 넘는다. UI의 200ms 스케줄링을 포함한 위 지연은 기존 문서의 50ms 목표를 달성한 값으로 해석하지 않는다.

## 재현과 원시 자료

```sh
python3 scripts/prepare_tauri_reference.py --help
python3 scripts/ui_fixture.py /tmp/everything-bench-new --entries 500000
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  cargo build --release --locked --features ui-automation --manifest-path src-tauri/Cargo.toml
python3 scripts/compare_gpui.py \
  --tauri /path/to/reference/src-tauri/target/release/everything \
  --fixture /tmp/everything-bench-new
```

- [측정/프로세스/전체 표본](gpui-evidence/performance/comparison.json)
- [검색 suite](gpui-evidence/performance/search-suite.json), [Tauri 엔진 결과](gpui-evidence/performance/tauri-engine.json), [GPUI 엔진 결과](gpui-evidence/performance/gpui-engine.json)
- [Tauri 측정 화면](gpui-evidence/performance/tauri-benchmark.png), [GPUI 측정 화면](gpui-evidence/performance/gpui-benchmark.png)
- [실제 UI TC와 시각 비교](gpui-test-cases.md)

운영 DB 전체, 실제 디스크 재색인, 배터리/발열, 장시간 안정성, Windows 메모리/응답은 이번 비교에 포함하지 않았다.
