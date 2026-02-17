# Everything 최적화 방안 통합 (검증 완료)

현재 코드베이스(`main.rs`, `fd_search.rs`, `query.rs`, `App.svelte`) 분석 및 교차 검증을 바탕으로 정리한다. 코드와 불일치하거나 이미 구현된 항목은 제거했다.

---

## 구현 완료 현황 (2026-02-17)

즉시 적용 7건 + 단기 5건 + 상수 변경 2건 + 프론트엔드 3건 = **총 19건 구현 완료**.

| 상태 | # | 항목 | 구현 내용 |
|:---:|---|------|-----------|
| DONE | 1-2 | BATCH_SIZE 증가 | 4,000 → 10,000 |
| DONE | 1-3 | metadata 이중 호출 제거 | `index_row_from_walkdir_entry()` 추가, `run_full_index` + `collect_rows_recursive` 적용 |
| DONE | 1-4+2-5 | FTS5 완전 제거 | FTS5 virtual table + 3 triggers 제거, DB_VERSION 2→3 범프 (1-4 트리거 비활성화와 2-5 제거 결정을 통합) |
| DONE | 2-1 | 인덱싱 전용 PRAGMA | `set_indexing_pragmas()` / `restore_normal_pragmas()` + 에러 경로 안전 보장 |
| DONE | 2-2 | WAL autocheckpoint 제어 | 인덱싱 중 0, 완료 후 TRUNCATE |
| DONE | 2-4 | 인덱스 추가 | `idx_entries_name_nocase`, `idx_entries_ext` + ANALYZE |
| DONE | 3-4 | ignore 규칙 캐시 | `IgnoreRulesCache` + mtime 기반 `cached_effective_ignore_rules()` |
| DONE | 3-5 | 확장자 특화 쿼리 | `SearchMode::ExtSearch` + `WHERE ext = ?1` (인덱스 활용) |
| DONE | 3-6 | NameSearch 2단계 쿼리 | UNION ALL (exact/prefix rank 0-1 + contains rank 2) |
| DONE | 4-1 | 라이브 검색 root 축소 | `find_child_dir_icase()` + PathSearch 시 첫 세그먼트로 root 축소 |
| DONE | 4-2 | 타임아웃 축소 | 10s → 5s |
| DONE | 4-3 | 깊이 제한 확대 | MAX_DEPTH 10 → 15 |
| DONE | 5-2 | 공통 확장자 프리로드 | 앱 시작 시 상위 20개 확장자 백그라운드 로드 |
| DONE | 6-1 | highlight 메모이제이션 | `highlightCache` Map + 쿼리 변경 시 초기화 |
| DONE | 6-2 | 가상 스크롤 버퍼 확대 | 상하 6→10행 (total buffer 12→20) |
| DONE | 6-3 | 검색 디바운스 leading edge | leading+trailing 200ms 하이브리드 |
| DONE | 7-1 | 디바운스 축소 | 500ms → 300ms |

---

## 1. 인덱싱 속도

### 1-1. 병렬 디렉터리 순회 (jwalk 활용)

| 항목 | 내용 |
|------|------|
| 현재 | `WalkDir::new(root)` 단일 스레드 순회 (`main.rs:1542`) |
| 문제 | $HOME 아래 수십만 파일을 한 스레드로 처리 |
| 방안 | `fd_search.rs`에서 이미 사용 중인 `jwalk::WalkDir`를 인덱싱에도 적용. `num_threads(4)` + 채널로 행을 수집하고, 메인 스레드에서 배치 upsert. DB write는 단일 트랜잭션 워커로 직렬화 |
| 기대 효과 | 2-3x (디스크 I/O 바운드이므로 CPU 병렬화 상한 있음) |
| 복잡도 | 중 |

### 1-2. BATCH_SIZE 적응형 조정 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `BATCH_SIZE = 4_000` 고정 (`main.rs:36`) |
| 문제 | 트랜잭션 커밋 횟수가 많아 I/O 오버헤드 발생 |
| 방안 | 파일 수/메모리 기반 8,000-20,000 가변. 메모리 부담 미미 (IndexRow ~300B × 16K ≈ 5MB) |
| 기대 효과 | 10-20% |
| 복잡도 | 하 |

### 1-3. metadata 이중 호출 제거 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `index_row_from_path()`가 `fs::symlink_metadata()` 재호출 (`main.rs:724`). `walkdir::WalkDir`는 내부적으로 `lstat` syscall을 이미 수행 |
| 문제 | 동일 파일에 대해 syscall 2회 발생. 단, macOS VFS 캐시 덕에 두 번째 호출은 커널 캐시 히트일 가능성 높음 |
| 방안 | `index_row_from_entry(entry: &walkdir::DirEntry)` 오버로드 추가, `entry.metadata()` 사용 |
| 기대 효과 | 5-15% (VFS 캐시 효과를 고려한 보수적 추정) |
| 복잡도 | 중 |

### 1-4. 인덱싱 중 FTS5 트리거 비활성화 — DONE (2-5와 통합: FTS5 완전 제거)

| 항목 | 내용 |
|------|------|
| 현재 | INSERT마다 `entries_ai` 트리거가 FTS5에 동기 삽입 (`main.rs:253-255`) |
| 문제 | 트리거 오버헤드가 전체 upsert 시간의 40-60%를 차지할 수 있음 |
| 방안 | full index 시 트리거 DROP → 배치 완료 후 `INSERT INTO entries_fts(entries_fts) VALUES('rebuild')` → 트리거 재생성 |
| 기대 효과 | 30-50% (bulk insert 기준) |
| 복잡도 | 중 |

### 1-5. 점진적(incremental) 인덱싱

| 항목 | 내용 |
|------|------|
| 현재 | `DELETE FROM entries` 후 전체 재스캔 (`main.rs:1487`) |
| 문제 | 앱 재시작마다 0부터 시작. 100만 파일 기준 수분 소요 |
| 방안 | 두 가지 접근 중 택 1: **(A)** `indexed_at` 타임스탬프 비교 — 스캔 시작 시각 기록 → 이미 존재하는 row는 mtime 비교 후 skip → 스캔 종료 후 `indexed_at < start_ts` 행만 DELETE. **(B)** `run_id` 컬럼 추가 후 이번 실행에서 touch 안 된 row만 삭제. 전체 `DELETE` 회피로 WAL 폭증 감소 |
| 기대 효과 | 극대 — 재시작 시 수초 내 Ready 전환 |
| 복잡도 | 상 |

---

## 2. SQLite 튜닝

### 2-1. 인덱싱 전용 PRAGMA 세트 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `synchronous=NORMAL`, `cache_size`/`mmap_size` 미설정 (`main.rs:194-199`) |
| 방안 | 인덱싱 시 `PRAGMA synchronous=OFF` + `PRAGMA cache_size = -65536` (64MB) + `PRAGMA mmap_size = 268435456` (256MB). 완료 후 `synchronous=NORMAL`, `cache_size = -16384` (16MB)로 복귀 |
| 기대 효과 | 쓰기 20-40% |
| 복잡도 | 하 |

### 2-2. WAL autocheckpoint 제어 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | WAL 기본 autocheckpoint (1000 페이지) |
| 방안 | 인덱싱 중 `PRAGMA wal_autocheckpoint = 0` → 완료 후 수동 `PRAGMA wal_checkpoint(TRUNCATE)` |
| 기대 효과 | 10-20% 쓰기 (checkpoint I/O 제거) |
| 복잡도 | 하 |

### 2-3. page_size 확대

| 항목 | 내용 |
|------|------|
| 현재 | 기본 4096B |
| 방안 | DB 생성 시 `PRAGMA page_size = 8192` (path 평균 길이 고려) |
| 기대 효과 | 5-15% I/O 감소 |
| 복잡도 | 중 (DB 버전 범프 필요) |

### 2-4. 인덱스 추가 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `idx_entries_dir`, `idx_entries_name`, `idx_entries_isdir`, `idx_entries_mtime` 개별 인덱스 (`main.rs:240-243`). ext 인덱스 없음 |
| 방안 | `CREATE INDEX idx_entries_name_nocase ON entries(name COLLATE NOCASE)` + `CREATE INDEX idx_entries_ext ON entries(ext)` + 대량 재인덱싱 후 `ANALYZE` 실행 |
| 비고 | ~~`idx_entries_dir_name` 복합 인덱스~~ PathSearch 쿼리가 `path LIKE`을 사용하므로(`main.rs:2116`) dir+name 복합 인덱스는 효과 없음 |
| 기대 효과 | NameSearch 10-20%, 확장자 검색 즉시 반환 |
| 복잡도 | 하 |

### 2-5. FTS5 활용 또는 제거 — DONE (B안: 완전 제거)

| 항목 | 내용 |
|------|------|
| 현재 | `entries_fts` 테이블이 트리거로 유지되지만(`main.rs:245-263`), **검색 커맨드에서 FTS5 MATCH 쿼리를 한 번도 사용하지 않음**. 모든 검색이 `entries` 테이블의 LIKE로만 동작 (`main.rs:2043-2131`) |
| 문제 | FTS5 테이블 + prefix 인덱스 5개 + 트리거 3개가 순수 오버헤드 |
| 방안 | **(A)** FTS5 MATCH를 검색 쿼리에 실제 활용 — prefix search에 LIKE 대신 FTS5 사용. 이 경우 `prefix='2 3'`으로 축소하면 빌드 시간/디스크 15-20% 절감. **(B)** FTS5를 완전 제거하여 인덱싱 시 트리거 오버헤드 영구 제거 |
| 기대 효과 | (A) 검색 성능 향상 + 인덱스 크기 절감 / (B) 인덱싱 30-50% 빠름 (1-4와 동일 효과) |
| 복잡도 | 중 |

---

## 3. 검색 응답 최적화

### 3-1. 검색어 기반 우선 인덱싱 (Query-First Indexing)

| 항목 | 내용 |
|------|------|
| 현재 | 검색 이력을 로그에만 기록 (`log_search`). 우선순위 인덱싱은 `DEFERRED_DIR_NAMES` 기반으로 일부 구현됨 (`main.rs:40, 1504-1534`) |
| 방안 | 최근 검색어 상위 N개를 `frequent_queries` 테이블에 유지. 검색어에서 루트 후보 추출 (`a_desktop`, `c_desktop/` 등) → 전체 인덱싱 전에 해당 subtree를 기존 priority_roots 앞에 배치. 상태바에 `Phase: Priority → Full` 표시 |
| 기대 효과 | 사용자 체감 검색 가용성 대폭 향상 |
| 복잡도 | 중 |

### 3-2. 쿼리별 결과 캐시 (인메모리)

| 항목 | 내용 |
|------|------|
| 현재 | `search` 커맨드(`main.rs:1971`)는 매 호출마다 SQL 실행. 결과 캐시 없음 |
| 방안 | LRU 캐시(크기 100, TTL 5초)에 `(query, sort, offset) → Vec<EntryDto>` 저장. `index_updated` 이벤트 시 무효화 |
| 기대 효과 | 동일 쿼리 반복 시 0ms 응답 |
| 복잡도 | 중 |

### 3-3. filter_ignored_entries를 DB 레벨로 이동

| 항목 | 내용 |
|------|------|
| 현재 | SQL 결과를 받은 후 Rust에서 필터링 (`main.rs:2147-2148`). `should_skip_path` → `matches_ignore_pattern` 호출 |
| 문제 | 쿼리가 LIMIT만큼 반환해도 필터링 후 결과가 줄어듦 |
| 방안 | ignore 패턴을 SQL WHERE 조건으로 변환하여 DB 레벨에서 배제 |
| 주의 | `IgnorePattern`에 `AnySegment`(`**/target` → 경로 내 임의 세그먼트 매치, `main.rs:151`)와 `Glob`(재귀 glob 매치, `main.rs:635-655`) 두 종류가 있음. 특히 `Glob` 패턴은 경로 접두사를 잘라가며 반복 매칭(`main.rs:657-670`)하므로 단순 `NOT LIKE`로 동치 변환이 어려움. `AnySegment`만 `path NOT LIKE '%/target' AND path NOT LIKE '%/target/%'`로 부분 변환 가능 |
| 기대 효과 | `AnySegment` 패턴 한정 20-40%. `Glob` 패턴은 Rust 후처리 유지 필요 |
| 복잡도 | 상 |

### 3-4. ignore 규칙 캐시 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `.pathignore`, `.gitignore` 파일을 `effective_ignore_rules()` 호출마다 파싱 (`main.rs:525` → `load_pathignore_rules` → `fs::read_to_string`) |
| 방안 | 파일 mtime 기반 캐싱. 변경 시에만 재로드, 요청마다 파싱하지 않음 |
| 기대 효과 | 검색 호출당 수백 μs 절감 |
| 복잡도 | 하 |

### 3-5. 확장자 특화 쿼리 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `*.png`는 `glob_to_like`에 의해 `name LIKE '%.png'`(suffix match)로 변환됨. 선행 `%`로 인해 인덱스 사용 불가 |
| 방안 | glob이 `*.ext` 패턴이면 `ext = 'png'`으로 변환. `idx_entries_ext` 인덱스 활용 |
| 기대 효과 | 확장자 검색 10-50x (인덱스 직접 탐색) |
| 복잡도 | 하 |

### 3-6. NameSearch 2단계 쿼리 + 랭킹 개선 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | CASE WHEN 3단계 rank 계산, 단일 쿼리 (`main.rs:2064-2069`) |
| 방안 | 1차: `name = 'q'` (exact) + `name LIKE 'q%'` (prefix) → 2차: `name LIKE '%q%'` (contains). `UNION ALL` + rank로 top 결과를 빠르게 반환. `name COLLATE NOCASE` 인덱스 활용 |
| 기대 효과 | 완전일치/접두사 일치 시 즉시 반환 |
| 복잡도 | 중 |

### 3-7. OFFSET → keyset 페이지네이션

| 항목 | 내용 |
|------|------|
| 현재 | `LIMIT ?1 OFFSET ?2` (`main.rs:2046, 2073, 2096, 2118`) |
| 문제 | OFFSET은 건너뛸 행을 모두 스캔 (O(n)) |
| 방안 | 이전 페이지 마지막 행의 `(sort_key, id)`를 기준으로 `WHERE (sort_key, id) > (?, ?)` |
| 기대 효과 | 5페이지 이후 50-80% 빠름 |
| 복잡도 | 중 |

### 3-8. 정렬 결과 클라이언트 캐싱

| 항목 | 내용 |
|------|------|
| 현재 | 정렬 변경 시 서버 재쿼리 (`App.svelte:560-572`) |
| 방안 | 결과 500건 이하면 프론트엔드에서 인메모리 정렬. 서버 왕복 제거 |
| 기대 효과 | 정렬 전환 50-100ms → 즉시 |
| 복잡도 | 중 |

### 3-9. path token 역색인 테이블

| 항목 | 내용 |
|------|------|
| 현재 | path 검색이 `e.path LIKE ?1`(`main.rs:2116`)로 full scan |
| 방안 | `entry_tokens(entry_id, token)` 구성 후 `token = 'a_desktop'`으로 후보 축소. `path LIKE '%...%'`를 token join 기반으로 대체 |
| 기대 효과 | path 검색 10x+ (LIKE full scan 제거) |
| 복잡도 | 상 |

---

## 4. 라이브 서치 (fd_search)

### 4-1. 라이브 검색 root 축소 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | 라이브 서치가 `$HOME` 전체를 탐색 (`fd_search.rs:250`) |
| 방안 | `a_desktop/ *.png` 같은 경로 검색 시 `$HOME` 전체 대신 `a_desktop` 하위만 검색 |
| 기대 효과 | 검색 범위 축소에 비례 (10-100x) |
| 복잡도 | 중 |

### 4-2. 타임아웃 축소 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `SEARCH_TIMEOUT = 10s` (`fd_search.rs:33`) |
| 방안 | 3-5초로 축소. 대부분의 유효 결과는 1초 내 수집됨 |
| 기대 효과 | 최악 케이스 대기 시간 50-70% 감소 |
| 복잡도 | 하 |

### 4-3. 깊이 제한 확대 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `MAX_DEPTH = 10` (`fd_search.rs:32`) |
| 방안 | 15-20으로 증가. node_modules 등은 이미 `should_skip_path`로 제외됨 |
| 기대 효과 | 깊은 프로젝트 구조에서 결과 누락 감소 |
| 복잡도 | 하 |

---

## 5. 아이콘 로딩

### 5-1. Swift 서브프로세스 → 상주 데몬

| 항목 | 내용 |
|------|------|
| 현재 | 확장자별 `Command::new("swift").arg("-e")` 프로세스 생성 (`main.rs:1878`) |
| 문제 | 프로세스 스폰 오버헤드 ~50-100ms/회 |
| 방안 | Swift CLI 바이너리 하나를 stdin/stdout 통신으로 상주시키거나, `objc` 크레이트로 NSWorkspace 직접 호출 |
| 기대 효과 | 5-10x 아이콘 로드 속도 |
| 복잡도 | 상 |

### 5-2. 공통 확장자 프리로드 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | 최초 요청 시 로드 |
| 방안 | 앱 시작 시 상위 20개 확장자(`txt`, `pdf`, `png`, `jpg`, `md`, `json`, `swift`, `rs`, `js`, `ts`, `html`, `css`, `py`, `zip`, `dmg`, `app`, `doc`, `xls`, `ppt`, `mov`)를 백그라운드에서 일괄 로드 |
| 기대 효과 | 첫 검색 결과 표시 시 아이콘 즉시 렌더 |
| 복잡도 | 하 |

### 5-3. 아이콘 배치 API

| 항목 | 내용 |
|------|------|
| 현재 | 보이는 행마다 `ensureIcon`을 `void` (fire-and-forget)로 호출하여 개별 비동기 요청 (`App.svelte:110`). 확장자당 1회 IPC 호출 (`App.svelte:176-196`). 이미 로딩 중인 확장자는 `iconLoading` Set으로 중복 방지됨 |
| 문제 | 확장자 종류가 많으면 동시에 다수의 IPC 호출 + Swift 프로세스가 발생 |
| 방안 | 미캐시 확장자를 프레임 단위로 모아 Rust 측 배치 API 1회로 일괄 처리 |
| 기대 효과 | IPC 오버헤드 감소, Swift 프로세스 수 제한 |
| 복잡도 | 중 |

---

## 6. 프론트엔드 렌더링

### 6-1. highlight 세그먼트 메모이제이션 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `highlightSegments()` 매 렌더 시 재계산 (`App.svelte:134-163`) |
| 문제 | 스크롤 시 동일 entry에 대해 반복 계산 |
| 방안 | `Map<string, Segment[]>` 캐시. 쿼리 변경 시 초기화 |
| 기대 효과 | 스크롤 성능 30-50% 개선 |
| 복잡도 | 하 |

### 6-2. 가상 스크롤 버퍼 확대 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | 상하 6행 버퍼 (`App.svelte:102-104`) |
| 방안 | 10-12행으로 증가. 빠른 스크롤 시 깜빡임 감소 |
| 기대 효과 | 체감 스크롤 부드러움 향상 |
| 복잡도 | 하 |

### 6-3. 검색 디바운스 leading edge 추가 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | 200ms trailing-edge 디바운스 (`App.svelte:408-413`) |
| 방안 | 첫 입력 시 즉시 전송 (leading edge) + 200ms trailing 유지. 첫 글자 입력 시 즉시 결과 반환 |
| 기대 효과 | 첫 글자 응답 체감 200ms 단축 |
| 복잡도 | 하 |

---

## 7. 파일 감시 (FSEvents watcher)

### 7-1. 디바운스 축소 — DONE

| 항목 | 내용 |
|------|------|
| 현재 | `WATCH_DEBOUNCE = 500ms` (`main.rs:38`) |
| 방안 | 200-300ms로 축소. 파일 저장 후 검색 반영까지 체감 지연 감소 |
| 기대 효과 | 사용자 체감 반응성 향상 |
| 복잡도 | 하 |

---

## 8. 메모리 관리

### 8-1. 아이콘 캐시 크기 제한

| 항목 | 내용 |
|------|------|
| 현재 | `HashMap<String, Vec<u8>>` 무한 성장 (`main.rs:165`). 키가 확장자이므로 실제 성장은 고유 확장자 수에 비례 (수백 개 수준) |
| 방안 | LRU 캐시 (최대 200 엔트리 또는 50MB) |
| 기대 효과 | 메모리 사용량 상한 보장. 실질적 리스크는 낮으므로 우선순위 하 |
| 복잡도 | 하 |

---

## 9. 측정 지표 (최적화와 반드시 함께 도입)

| 지표 | 설명 |
|------|------|
| Full indexing throughput | files/sec, rows/sec, total duration |
| Search latency | p50/p95 (`name`, `path`, `glob` 유형별) |
| First useful result time | 앱 시작 후 첫 유효 결과까지 시간 |
| Ignore hit ratio | 스캔 중 skip된 엔트리 비율 |

---

## 우선순위 매트릭스

### 즉시 적용 (하루 이내) — 7/7 DONE

| # | 항목 | 기대 효과 | 상태 |
|---|------|-----------|:---:|
| 2-1 | SQLite 인덱싱 전용 PRAGMA 세트 | 쓰기 20-40% | DONE |
| 2-2 | WAL autocheckpoint 제어 | 쓰기 10-20% | DONE |
| 2-4 | name_nocase + ext 인덱스 | NameSearch 10-20%, ext 즉시 | DONE |
| 1-2 | BATCH_SIZE 증가 | 인덱싱 10-20% | DONE |
| 3-4 | ignore 규칙 캐시 | 검색당 수백 μs | DONE |
| 3-5 | 확장자 특화 쿼리 | 확장자 검색 10-50x | DONE |
| 5-2 | 공통 확장자 프리로드 | 첫 검색 체감 | DONE |

### 단기 (1주 이내) — 5/7 DONE

| # | 항목 | 기대 효과 | 상태 |
|---|------|-----------|:---:|
| 1-4 | 인덱싱 중 FTS5 트리거 비활성화 | 인덱싱 30-50% | DONE (FTS5 완전 제거) |
| 2-5 | FTS5 활용 또는 제거 결정 | 인덱싱 or 검색 개선 | DONE (B안: 완전 제거) |
| 1-3 | metadata 이중 호출 제거 | 인덱싱 5-15% | DONE |
| 3-1 | 검색어 기반 우선 인덱싱 | 체감 가용성 | — |
| 3-6 | NameSearch 2단계 쿼리 | 완전일치 즉시 반환 | DONE |
| 6-1 | highlight 메모이제이션 | 스크롤 30-50% | DONE |
| 4-1 | 라이브 검색 root 축소 | 범위 비례 10-100x | DONE |

### 중기 (1개월 이내)

| # | 항목 | 기대 효과 |
|---|------|-----------|
| 1-1 | jwalk 병렬 순회 | 인덱싱 2-3x |
| 1-5 | 점진적 인덱싱 | 재시작 수초 |
| 3-7 | keyset 페이지네이션 | 깊은 페이지 50-80% |
| 3-9 | path token 역색인 | path 검색 10x+ |
| 5-1 | Swift 데몬 / objc 직접 호출 | 아이콘 5-10x |
| 3-2 | 쿼리 결과 인메모리 캐시 | 반복 쿼리 0ms |
| 3-3 | ignore 일부를 SQL WHERE로 | AnySegment 한정 20-40% |

### 실행 순서 제안

1. 즉시 적용 항목 전부 + baseline 계측 지표 도입
2. FTS5 활용 여부 결정 (2-5) — 이후 방향에 영향
3. Query-First 인덱싱 (3-1) 적용
4. NameSearch 2단계 + 확장자 특화 쿼리 (3-6, 3-5)
5. FTS5 트리거 비활성화 + metadata 제거 (1-4, 1-3)
6. token 역색인 (3-9)으로 path 검색 고속화
7. jwalk 병렬 순회 + 점진적 인덱싱 (1-1, 1-5)

---

## 부록: 검증 과정에서 제거된 항목

| 원래 번호 | 항목 | 제거 사유 |
|-----------|------|-----------|
| 4-4 | 중복 정렬 제거 | `fd_search`와 `search`는 별개 IPC 커맨드. 중복 정렬 전제가 사실과 다름 |
| 7-2 | pending_paths 중복 제거 | 이미 `HashSet<PathBuf>`로 구현됨 (`main.rs:1783`) |
| 8-2 | 라이브 서치 캐시 120→50 | 해당 캐시는 `start_live_search_worker` 내부(`main.rs:1266`)에 있으나, 프론트엔드는 `fd_search` 커맨드(`App.svelte:442`)를 호출하며 이는 단일 `Option<FdSearchCache>`(`main.rs:169`)를 사용. 120개 캐시가 있는 경로는 사실상 dead code |
