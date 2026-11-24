# Missing files in newly created Windows directories

## Diagnosis (2026-09-16)

`h3_module_spec_scenario.html` existed under
`c_desktop/tools/desktop-use-cli/tc/scenarios/`, but the service's SQLite index
contained no row for the file or any of its siblings. The `scenarios` directory
itself was present with `run_id = 0` (inserted by the watcher); its parent had
`run_id = 9` (the previous full scan). The service reported `Ready`, continued
advancing its USN cursor, and no exclusion rule matched the path.

This isolates the problem to resolving parents created after the MFT scan:
the directory creation event uses a cached parent, while events for its children
must fall back to `OpenFileById`.

## Causes and correction

- That fallback previously passed a masked 48-bit MFT record index to
  `OpenFileById`, losing the required sequence number. The existing working-tree
  fix preserves the full parent FRN for the syscall and the masked FRN for caches.
- A second bug remained after that fix: the watcher appended `\` to the scan
  root `C:\`, producing a prefix with two trailing separators. Ordinary resolved
  paths failed the prefix comparison and were treated as outside the scan.
  The watcher now compares `Path` components with `starts_with`.
- Failed lookups were also cached permanently as out-of-scope parents. Only
  successfully resolved, out-of-scope paths are now put in that cache. A lookup
  failure leaves the parent eligible for resolution on a later journal event.

No indexing scope, exclusion rules, permissions, or database schema changed.

## Verification and recovery

The watcher tests cover drive roots, alternate/trailing separators, exact roots,
similarly named siblings, other drives, retry after lookup failure, and a real
NTFS directory resolved by its full file ID. All nine watcher tests pass.
Reinstating the old prefix comparison makes the drive-root regression fail.

The full Rust suite reports 104 passed, four failed, and one ignored. The same
four failures reproduce on the original `HEAD` watcher: three `rescan` tests
using slash-based paths and `mcp_server::tests::search_name_glob_ext_path_modes`.

The old watcher has already consumed the journal events for missing files, so
repair requires a full reindex as well as the code update. Use the service's
`reset_index` command to reconcile existing files. Install the rebuilt app to
apply the watcher fix to future directory creation; reindexing alone only
repairs the current omissions.

Recovery was verified on the local installation: after `reset_index` finished,
the installed executable's MCP `search` returned exactly one result for
`h3_module_spec_scenario.html` at its expected path. The fixed NSIS installer was
built at `src-tauri/target/release/bundle/nsis/Everything_0.5.3_x64-setup.exe`;
the running installed service has not yet been replaced with that build.
