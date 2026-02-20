Windows NTFS MFT + USN Journal 구현 계획

Context

현재 Windows에서는 앱 시작 시 WalkDir로 전체 스캔만 하고, 파일 변경 감지(watcher)가 없음.
macOS의 FSEvents처럼 NTFS 네이티브 API를 활용해 초고속 초기 인덱싱(MFT)과 실시간 변경
감지(USN Journal)를 구현한다.

인메모리 인덱스는 이번 스코프에서 제외 — 기존 SQLite FTS5 검색 유지

인덱싱 범위: C: 드라이브만 (home_dir 하위 필터링)

MFT 실패 시: 기존 WalkDir fallback 자동 전환

신규 파일 구조

src-tauri/src/
win/
mod.rs -- Windows 모듈 루트 (cfg(target_os = "windows"))
mft_indexer.rs -- MFT 직접 읽기 → SQLite upsert
usn_watcher.rs -- USN Change Journal 폴링 → DB 업데이트
volume.rs -- NTFS 볼륨 핸들 열기
path_resolver.rs -- MFT FRN(File Reference Number) → 전체 경로 변환

Phase 1: 기반 — 볼륨 핸들 + 모듈 구조

1-1. Cargo.toml Windows 전용 의존성 추가

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = [
"Win32_Storage_FileSystem",
"Win32_System_IO",
"Win32_System_Ioctl",
"Win32_Foundation",
] }

usn-journal-rs는 API 안정성 확인 후 사용 여부 결정. 안전하게 windows crate 직접 사용으로 시작.

1-2. win/mod.rs — 모듈 선언

#[cfg(target_os = "windows")]
pub mod volume;
#[cfg(target_os = "windows")]
pub mod path_resolver;
#[cfg(target_os = "windows")]
pub mod mft_indexer;
#[cfg(target_os = "windows")]
pub mod usn_watcher;

1-3. win/volume.rs — NTFS 볼륨 핸들

open_volume(drive_letter: char) -> Result<HANDLE>: CreateFileW("\.\C:") 호출

query_usn_journal(handle) -> Result<UsnJournalData>: FSCTL_QUERY_USN_JOURNAL

권한 실패 시 Err 반환 (호출측에서 WalkDir fallback)

1-4. main.rs 수정 — 모듈 선언 + setup_app 분기

기존 (line 4244):

#[cfg(not(target_os = "macos"))]
{
let _ = start_full_index_worker(app_handle.clone(), state.clone());
}

변경:

#[cfg(target_os = "windows")]
{
win::start_windows_indexing(app_handle.clone(), state.clone());
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
{
let _ = start_full_index_worker(app_handle.clone(), state.clone());
}