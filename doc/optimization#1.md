# Optimization #1

## 목적
- 인덱싱 총 소요 시간 단축
- 앱 실행 직후 검색 체감 속도 개선
- 사용자가 방금 검색한 키워드/경로를 우선적으로 찾도록 개선

## 현재 병목 요약
- 전체 인덱싱이 `HOME` 전역 스캔 기반이라 cold start 비용이 큼
- `name LIKE '%...%'`, `path LIKE '%...%'` 형태가 많아 인덱스 효율이 낮음
- full reindex 시 `DELETE + 전체 재삽입` 구조라 I/O 부하가 큼
- 검색 시마다 ignore 규칙 재로딩/재평가 비용이 있음

## 우선순위별 최적화 항목

## P0 (즉시, 1~2일)
1. hot path 디버그 로깅 비활성화  
   - `src-tauri/src/main.rs`의 per-entry 로그는 기본 off, 필요 시 env flag로만 활성화.
2. full index 전용 DB PRAGMA 튜닝  
   - 인덱싱 중 `synchronous=OFF`, 완료 후 `NORMAL` 복귀.
   - `cache_size`, `mmap_size` 튜닝으로 write throughput 개선.
3. 배치 크기 적응형 조정  
   - `BATCH_SIZE`를 고정 4,000에서 파일 수/메모리 기반 8,000~20,000 가변.
4. ignore 규칙 캐시  
   - `.pathignore`, `.gitignore` 파일 mtime 기반으로 규칙 캐싱.
   - 검색 요청마다 파싱하지 않고 변경 시에만 재로드.
5. 확장자 특화 쿼리  
   - `*.png`는 `name LIKE` 대신 `ext='png'` 경로 사용.
   - `CREATE INDEX idx_entries_ext ON entries(ext)` 추가.

## P1 (단기, 1주)
1. Query-First 인덱싱(가장 중요)  
   - 사용자 검색어에서 루트 후보 추출(`a_desktop`, `c_desktop`, `a_desktop/`).
   - 전체 인덱싱 전에 우선 큐로 해당 subtree 선인덱싱.
   - 상태바에 `Phase: Priority Indexing -> Full Indexing` 표시.
2. 최근 검색 기반 우선순위  
   - `search.log`/내부 메모리에서 최근 N개 키워드 집계.
   - 앱 재시작 시 상위 경로 먼저 스캔.
3. Name 검색 2단계 쿼리  
   - 1차: exact/prefix(`name='q'`, `name LIKE 'q%'`)
   - 2차: contains(`'%q%'`)
   - `UNION ALL` + rank로 top 결과를 빠르게 반환.
4. Live 검색 root 축소  
   - `a_desktop/ *.png`는 `HOME` 전체 대신 `a_desktop` 하위만 검색.

## P2 (중기, 2~4주)
1. path token 역색인 테이블  
   - `entry_tokens(entry_id, token)` 구성 후 `token='a_desktop'`로 후보 축소.
   - `path LIKE '%...%'`를 token join 기반으로 대체.
2. FTS5 본격 활용  
   - 현재 생성된 `entries_fts`를 실제 검색 플로우에 연결.
   - prefix/token 검색은 FTS, 최종 정렬은 rank + 기존 정렬 혼합.
3. full reindex 구조 개선  
   - `run_id` 컬럼 추가 후 이번 실행에서 touch 안 된 row만 삭제.
   - 전체 `DELETE` 회피로 WAL 폭증 감소.
4. 병렬 스캔 + 단일 writer 파이프라인  
   - 스캔 스레드 다중화, DB write는 단일 트랜잭션 워커로 직렬화.

## 추천 인덱스/스키마
- `CREATE INDEX idx_entries_name_nocase ON entries(name COLLATE NOCASE);`
- `CREATE INDEX idx_entries_ext ON entries(ext);`
- `CREATE INDEX idx_entries_dir_name ON entries(dir, name COLLATE NOCASE);`
- 대량 재인덱싱 후 `ANALYZE;` 실행

## 측정 지표(반드시 같이 도입)
- Full indexing: files/sec, rows/sec, total duration
- Search latency: p50/p95 (`name`, `path`, `glob` 유형별)
- First useful result time: 앱 시작 후 첫 유효 결과까지 시간
- Ignore hit ratio: 스캔 중 skip된 엔트리 비율

## 실행 순서 제안
1. P0 전부 적용 + baseline 계측
2. Query-First 인덱싱(P1-1) 먼저 적용
3. Name/Ext 쿼리 최적화(P1-3, P0-5)
4. Token 역색인(P2-1)로 path 검색 고속화

