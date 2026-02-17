# UI-less Index/Search Benchmark (2026-02-17)

## 목적
- UI 없이 앱 실제 실행 경로에서 인덱싱/검색 성능 측정
- 1회차(cold)와 재실행(warm) 비교
- 사용자형 검색어 TC별 결과 도달 시간 기록

## 코드 변경(측정용)
- 파일: `src-tauri/src/main.rs`
- 추가 사항:
  - `EVERYTHING_BENCH=1` 벤치 모드 추가
  - 인덱싱/검색 성능 로그(`[perf]`) 추가
  - 인덱싱 완료 대기 후 검색 TC 자동 실행
  - 결과를 JSON 리포트로 저장 (`EVERYTHING_BENCH_OUTPUT`)
  - `EVERYTHING_BENCH_EXIT=1` 시 자동 종료

## 데이터셋
- 경로: `/tmp/everything-bench-home.tynS4K`
- 파일 수: 39,009
- 용량: 152MB
- 주요 구조: `Desktop`, `Documents`, `Downloads`, `Projects`, `Archive` 등

## 실행 명령

### Cold run
```bash
HOME=/tmp/everything-bench-home.tynS4K \
RUSTUP_HOME=/Users/al02402336/.rustup \
CARGO_HOME=/Users/al02402336/.cargo \
EVERYTHING_BENCH=1 \
EVERYTHING_BENCH_EXIT=1 \
EVERYTHING_BENCH_RUN_LABEL=cold \
EVERYTHING_BENCH_ITERATIONS=5 \
EVERYTHING_BENCH_OUTPUT=/tmp/everything-bench-cold.json \
FASTFIND_PERF_LOG=1 \
cargo run --manifest-path src-tauri/Cargo.toml
```

### Warm run (재실행)
```bash
HOME=/tmp/everything-bench-home.tynS4K \
RUSTUP_HOME=/Users/al02402336/.rustup \
CARGO_HOME=/Users/al02402336/.cargo \
EVERYTHING_BENCH=1 \
EVERYTHING_BENCH_EXIT=1 \
EVERYTHING_BENCH_RUN_LABEL=warm \
EVERYTHING_BENCH_ITERATIONS=5 \
EVERYTHING_BENCH_OUTPUT=/tmp/everything-bench-warm.json \
FASTFIND_PERF_LOG=1 \
cargo run --manifest-path src-tauri/Cargo.toml
```

## 인덱싱 결과
| Run | indexWaitMs | indexScanned | indexIndexed | indexEntriesCount | permissionErrors |
|---|---:|---:|---:|---:|---:|
| cold | 2703 | 39058 | 39047 | 39058 | 0 |
| warm | 1857 | 39059 | 5 | 39059 | 0 |

해석:
- warm에서 DB upsert는 거의 없음(`indexIndexed=5`)에도 전체 파일 스캔(`indexScanned`)은 거의 동일하게 수행됨.

## 검색 TC 및 평균 지연(ms)
(각 TC 5회 평균, limit=300)

| TC | Query | Mode | Cold ms | Warm ms | Result count |
|---|---|---|---:|---:|---:|
| TC01 | `report_00042` | name | 1.595 | 1.728 | 2 |
| TC02 | `report_00` | name | 3.867 | 4.152 | 300 |
| TC03 | `invoice` | name | 3.804 | 3.863 | 300 |
| TC04 | `*.md` | ext | 3.728 | 3.982 | 300 |
| TC05 | `Desktop/ *.png` | path | 1.250 | 1.472 | 2 |
| TC06 | `Projects/rust` | path | 4.056 | 4.058 | 1 |
| TC07 | `Projects/ *.rs` | path | 9.133 | 9.245 | 300 |
| TC08 | `zzzz_not_exists_12345` | name | 7.623 | 7.479 | 0 |

모든 TC `passed=true`.

## 느린 케이스 원인 추적

### TC07 (`Projects/ *.rs`)이 가장 느림 (~9ms)
- 경로: `PathSearch`
- 실행 경로:
  - `dir` 범위 조건 + `name LIKE %.rs` + `ORDER BY name`
  - 조건에 맞는 `.rs` 후보군이 많고(limit 300), 정렬 비용이 발생
- 로그 근거: cold/warm 모두 9ms대에서 안정적 재현

### TC08 (`zzzz_not_exists_12345`) no-match가 상대적으로 느림 (~7.5ms)
- 경로: `NameSearch`
- 실행 경로:
  - exact/prefix가 0건일 때 Phase2 contains 검색(`LIKE %...%`) 수행
  - no-match에서는 후보 전반을 확인 후 종료되어 시간 증가
- 로그 근거: cold/warm 모두 7.4~7.6ms

### 재실행(warm)에서도 인덱싱 시간이 유의미하게 남는 이유
- 경로: `run_incremental_index_inner`
- 설계상 모든 파일을 순회하여 `mtime/size`를 비교하고 run_id를 갱신
- 변경 파일이 거의 없어도 디스크 traversal/stat 비용은 대부분 유지

## 산출물
- cold report: `/tmp/everything-bench-cold.json`
- warm report: `/tmp/everything-bench-warm.json`
