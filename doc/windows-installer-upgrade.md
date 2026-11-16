# Windows installer service handover

The installer now handles a running or stuck `EverythingIndexer` before
replacing the application. A previous install could stop at the last visible
`everything_killpath.ps1` deletion message, then fail with
`EverythingIndexer did not stop within 30s`: its preinstall hook only sent one
stop request and polled, with no verified force-stop fallback.

`windows/prepare-upgrade.ps1` runs elevated from NSIS's private plugin directory:

1. Read the registered service image, PID and original startup configuration.
2. Temporarily disable service startup so SCM recovery and GUI auto-start cannot
   relaunch the old executable during replacement.
3. Request a graceful stop. The `sc.exe` request is bounded to three seconds,
   followed by at most five seconds for the service to drain.
4. If it is still alive, hold a process handle, verify its image matches the
   registered indexer, terminate that process, and confirm the service stopped.
   An unverified process or failed termination aborts installation.
5. Close clients whose exact executable matches the selected install or
   registered service path. A different application named Everything is kept.
6. Copy the stopped index's `index.db`, `index.db-wal` and `index.db-shm`, when
   present, into `%ProgramData%\com.everything.app\data\upgrade-backups\<time-guid>`.
   The original files are preserved. Backup failure aborts replacement.
7. Replace files, restore delayed automatic startup, and verify the new service
   is running. A failure or cancellation restores the previous startup setting.

The helper uses WMI for executable paths and does not rely on
`Get-Process.MainModule`, which is unsuitable for an x86 NSIS host inspecting
x64 clients. Termination is scoped to verified PIDs and handles.

Windows bulk indexing keeps `journal_mode=WAL` and `synchronous=NORMAL`.
An interrupted uncommitted batch can be rolled back; keeping the WAL and SHM
alongside the database lets SQLite recover on the next writer open. This is
process-crash protection, not a guarantee against disk failure or every possible
OS/storage fault. The preserved backup is available for diagnosis or recovery.

Starting the new indexer enqueues an unconditional full build. Existing Temp
documents are therefore discovered after upgrading to the version that removes
the built-in Temp exclusions; explicit `.pathignore` rules still apply.

## Validation

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test-windows-installer.ps1
& "$env:SystemRoot\SysWOW64\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -File scripts/test-windows-installer.ps1
cargo test --release --locked --manifest-path app/Cargo.toml -- --test-threads=1
./scripts/build-windows.ps1
```

Both PowerShell hosts passed nine checks: fresh install, graceful stop, hung
writer fallback, failed termination, a surviving writer, startup restoration,
unexpected images, exact DB/WAL/SHM backup bytes, and real termination of an
owned x64 worker after rejecting an image mismatch. Service operations in these
checks are simulated; they never stop/register the production service.

The Rust suite passed 127 tests. The Windows crash regression writes a committed
row and spills an uncommitted large transaction to WAL in a child process,
terminates it with `TerminateProcess`, reopens the fixture, and checks that
`PRAGMA quick_check` returns `ok` and only the committed row remains.

Production installation and its UAC interaction are performed by running the
rebuilt installer, rather than by these fixture checks.
