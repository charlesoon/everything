# 증분 인덱싱 + FSEvents 재생 구현 계획

## 목표

앱 종료 중 발생한 파일 변경을 재시작 시 반영한다. 현재의 `DELETE FROM entries` + full rebuild를 제거하고, 기존 인덱스를 즉시 사용 가능하게 한다.

```
시작
 ├─ last_event_id 있음 → FSEvents replay → 성공 → Ready
 │                                        → overflow → fallback ─┐
 └─ last_event_id 없음 ─────────────────────────────────────────→ │
                                                                   ▼
                                                        증분 reconciliation
                                                        (full walk + mtime 비교)
                                                               → Ready
```

## 현재 구조 (문제점)

- `run_full_index()` 시작 시 `DELETE FROM entries` 실행 (`main.rs`)
- 매 실행마다 전체 트리를 WalkDir로 순회하며 모든 row를 upsert
- `indexed_at` 컬럼은 존재하나 증분 판별에 사용되지 않음
- notify watcher는 `SinceNow` 고정 — 오프라인 이벤트 수신 불가

## 의존성

- `fsevent-sys 4.1.0` — notify 8의 transitive dep으로 이미 Cargo.lock에 존재
- 추가 크레이트 불필요 (fsevent-sys를 직접 사용)

---

## 수정 대상 파일

| 파일 | 변경 내용 |
|------|-----------|
| `src-tauri/Cargo.toml` | `fsevent-sys` 직접 의존성 추가 |
| `src-tauri/src/main.rs` | DB 스키마, 인덱싱 로직, watcher 교체, startup 플로우 |
| `src-tauri/src/fsevent_watcher.rs` | **신규** — FSEvents 직접 바인딩 watcher |

---

## 구현 단계

### Step 1. DB 스키마 변경

`DB_VERSION` 3 → 4 범프. `init_db()`에 `meta` 테이블 추가.

```sql
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
```

`entries` 테이블에 `run_id` 컬럼 추가:

```sql
ALTER TABLE entries ADD COLUMN run_id INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_entries_run_id ON entries(run_id);
```

기존 version-bump 로직(DROP → 재생성)이 마이그레이션 자동 처리.

`meta` 테이블에 저장할 키:

| key | 값 | 용도 |
|-----|---|------|
| `last_event_id` | FSEvents event ID (u64) | 재시작 시 replay 시작점 |
| `last_run_id` | 마지막 성공한 run_id (u64) | 증분 reconciliation 용 |

### Step 2. 증분 reconciliation 로직 (`run_incremental_index`)

`run_full_index()`를 `run_incremental_index()`로 교체. **DELETE FROM entries 제거**.

```
fn run_incremental_index(app, state) -> AppResult<()>:
  1. conn = db_connection()
     set_indexing_pragmas()
  2. current_run_id = last_run_id + 1
  3. WalkDir 순회:
     for entry in walk:
       - DB에서 path로 기존 row 조회 (SELECT mtime, size FROM entries WHERE path = ?1)
       - 기존 row 없음 → INSERT (run_id = current_run_id)
       - 기존 row 있고 mtime/size 동일 → UPDATE run_id만 갱신 (경량 UPDATE)
       - 기존 row 있고 mtime/size 변경 → full UPDATE (run_id = current_run_id)
  4. 순회 완료 후:
     DELETE FROM entries WHERE run_id < current_run_id
     (= 이번 실행에서 미탐색 = 삭제된 파일)
  5. meta 테이블에 last_run_id = current_run_id 저장
  6. ANALYZE + restore_normal_pragmas()
```

최적화 포인트:
- mtime/size 비교 시 DB 조회를 매 파일마다 하지 않고, **배치 단위**로 기존 row를 HashMap에 프리로드하여 인메모리 비교
- `run_id` UPDATE는 prepared statement 재사용
- 변경 없는 파일은 `UPDATE entries SET run_id = ?1 WHERE path = ?2` — 최소 I/O

### Step 3. FSEvents 직접 바인딩 watcher (`fsevent_watcher.rs`)

`fsevent-sys`를 직접 사용하여 `since_when` 파라미터를 제어하는 watcher 구현.

```rust
// fsevent_watcher.rs (신규 파일)

pub struct FsEventWatcher {
    stream: fsevent_sys::FSEventStreamRef,
    last_event_id: Arc<AtomicU64>,
    tx: mpsc::Sender<Vec<PathBuf>>,
}

impl FsEventWatcher {
    /// since_event_id가 Some이면 해당 시점부터 replay
    /// None이면 kFSEventStreamEventIdSinceNow
    pub fn new(
        root: &Path,
        since_event_id: Option<u64>,
        tx: mpsc::Sender<Vec<PathBuf>>,
    ) -> Result<Self, String>;

    /// 현재까지 수신한 최신 event_id 반환
    pub fn last_event_id(&self) -> u64;

    pub fn stop(&mut self);
}
```

FSEvents 콜백에서 처리할 플래그:
- `kFSEventStreamEventFlagMustScanSubDirs` → 해당 subtree 재스캔 신호 전송
- `kFSEventStreamEventFlagRootChanged` → root 변경 감지
- 일반 이벤트 → 변경 경로를 채널로 전송

### Step 4. startup 플로우 변경 (`setup_app`)

현재 플로우:
```
init_db → purge_ignored → db_ready=true → start_watcher → start_full_index
```

변경 후:
```
init_db → purge_ignored → db_ready=true → emit_status_counts
→ last_event_id = meta에서 조회
→ if last_event_id 존재:
    start_fsevent_watcher(since: last_event_id)
    replay 완료 대기 (replay 중 수신 이벤트 → apply_path_changes)
    overflow 발생 시 → fallback to run_incremental_index
  else:
    run_incremental_index (최초 실행 또는 DB reset)
    start_fsevent_watcher(since: now)
→ 주기적으로 meta.last_event_id 갱신 (watcher의 last_event_id)
```

핵심: **db_ready=true를 인덱싱 전에 설정**하여 기존 인덱스로 즉시 검색 가능.

### Step 5. event_id 영속화

watcher가 이벤트를 처리할 때마다 `last_event_id`를 메모리에 갱신하고, 주기적으로(30초 또는 앱 종료 시) DB meta 테이블에 flush.

```rust
fn persist_event_id(db_path: &Path, event_id: u64) -> AppResult<()> {
    let conn = db_connection(db_path)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES('last_event_id', ?1)",
        params![event_id.to_string()],
    ).map_err(|e| e.to_string())?;
    Ok(())
}
```

Tauri의 `on_exit` 훅 또는 `Drop` impl로 앱 종료 시 최종 flush 보장.

### Step 6. notify → FsEventWatcher 전환

`start_watcher_worker()`에서 `notify::RecommendedWatcher` 대신 Step 3의 `FsEventWatcher` 사용.

이벤트 처리 루프는 기존 debounce 로직(`pending_paths` + `WATCH_DEBOUNCE`) 유지.

`kFSEventStreamEventFlagMustScanSubDirs` 수신 시:
```rust
// overflow 경로의 subtree만 재스캔
let rows = collect_rows_recursive(&overflow_path, &ignored_roots, &ignored_patterns);
upsert_rows(&mut conn, &rows)?;
```

### Step 7. 기존 full rebuild 경로 정리

- `run_full_index()` / `run_full_index_inner()` → `run_incremental_index()` / `run_incremental_index_inner()`로 교체
- `start_full_index` IPC 커맨드 유지 (수동 reset 용도)
- `reset_index` IPC 커맨드: `DELETE FROM entries` + `DELETE FROM meta` + `run_incremental_index` 호출

---

## 기대 효과

| 시나리오 | 현재 | 개선 후 |
|---------|------|---------|
| 앱 시작 (변경 없음) | full rebuild 수분 | FSEvents replay ~100ms, Ready 즉시 |
| 앱 시작 (소수 변경) | full rebuild 수분 | replay 변경분만 ~500ms |
| 앱 시작 (대량 변경/첫 실행) | full rebuild 수분 | 증분 reconciliation, 변경분만 write |
| 앱 시작 → 검색 가능 시점 | full rebuild 완료 후 | 즉시 (기존 인덱스 사용) |

## 위험 요소 및 대응

| 위험 | 대응 |
|------|------|
| FSEvents event_id가 시스템 재시작으로 reset | `last_event_id` 유효성 검사 — FSEvents가 `kFSEventStreamEventFlagHistoryDone` 없이 바로 overflow 보내면 fallback |
| fsevent-sys 직접 사용 시 unsafe 코드 | 최소한의 unsafe 래퍼, 메모리 관리 명확히 |
| 증분 reconciliation에서 mtime 비교 누락 (macOS는 1초 해상도) | size도 함께 비교. 동일 초 내 변경은 다음 watcher 이벤트로 보완 |
| 대량 삭제 시 `DELETE WHERE run_id < ?` 느림 | 인덱스 `idx_entries_run_id`로 커버 |
| notify 크레이트 제거 시 Linux/Windows 미지원 | `#[cfg(target_os = "macos")]`로 분기, 비macOS는 기존 notify 유지 |

## 테스트 계획

1. **단위 테스트**: `run_incremental_index` — 임시 DB에 기존 데이터 삽입 후, 파일 추가/삭제/변경 시나리오 검증
2. **통합 테스트**: FSEvents watcher — 임시 디렉토리에서 파일 변경 후 이벤트 수신 확인
3. **수동 검증**: 앱 실행 → 파일 변경 → 앱 종료 → 재실행 → 변경 반영 확인
4. **경계 테스트**: event_id overflow, 첫 실행, DB reset 후 재실행

## 구현 순서 (의존성 기반)

```
Step 1 (DB 스키마) ──→ Step 2 (증분 reconciliation) ──→ Step 7 (정리)
                   ├─→ Step 3 (FsEventWatcher)       ──→ Step 6 (전환)
                   └─→ Step 4 (startup 플로우)
                        Step 5 (event_id 영속화)
```

Step 1 완료 후 Step 2·3을 병렬 진행 가능. Step 4·5는 Step 2·3 완료 후.
