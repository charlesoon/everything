[CmdletBinding()]
param(
    [ValidateSet('Prepare', 'Restore')]
    [string]$Mode = 'Prepare',
    [string]$InstallDir,
    [string]$StateFile
)

$ErrorActionPreference = 'Stop'
$script:IndexerServiceName = 'EverythingIndexer'

function Get-IndexerService {
    Get-CimInstance Win32_Service -Filter "Name='$script:IndexerServiceName'" -ErrorAction Stop
}

function Get-NormalizedExecutablePath([string]$Path) {
    $expanded = [Environment]::ExpandEnvironmentVariables($Path)
    if (-not [IO.Path]::IsPathRooted($expanded)) { throw "Expected an absolute executable path: $Path" }
    [IO.Path]::GetFullPath($expanded)
}

function Get-IndexerExecutablePath([string]$CommandLine) {
    if ($CommandLine -notmatch '(?:^|\s)--service(?:\s|$)') { throw 'Unexpected indexer service command line' }
    if ($CommandLine -match '^"([^"]+)"') {
        $binary = $Matches[1]
    } elseif ($CommandLine -match '^(.*?\.exe)(?:\s|$)') {
        $binary = $Matches[1]
    } else { throw 'Cannot determine the registered indexer executable' }
    $binary = Get-NormalizedExecutablePath $binary
    if ([IO.Path]::GetFileName($binary) -ine 'Everything.exe') { throw "Unexpected indexer executable: $binary" }
    $binary
}

function Stop-ValidatedUpgradeProcess([uint32]$TargetProcessId, [string]$ExpectedPath) {
    $target = Get-Process -Id $TargetProcessId -ErrorAction SilentlyContinue
    if (-not $target) { return }
    $debugPrivilege = $false
    try {
        # The registered service runs as SYSTEM. The elevated installer may
        # enable its existing debug privilege to open that process; owned test
        # workers also work without this privilege in an unelevated session.
        try { [Diagnostics.Process]::EnterDebugMode(); $debugPrivilege = $true } catch {}
        # Hold a handle before checking the image, so PID reuse cannot redirect
        # Kill(). Do not use MainModule/Path: a 32-bit NSIS host must handle x64.
        $null = $target.Handle
        $details = Get-CimInstance Win32_Process -Filter "ProcessId=$TargetProcessId" -ErrorAction Stop
        if ($target.HasExited) { return }
        if (-not $details.ExecutablePath) { throw "Cannot verify image for PID $TargetProcessId" }
        $actual = Get-NormalizedExecutablePath $details.ExecutablePath
        if (-not [string]::Equals($actual, $ExpectedPath, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to terminate PID $TargetProcessId with a different executable: $actual"
        }
        Write-Output "Force-stopping verified process PID $TargetProcessId ($actual)"
        $target.Kill()
        if (-not $target.WaitForExit(5000)) { throw "PID $TargetProcessId did not exit within 5 seconds" }
    } finally {
        $target.Dispose()
        if ($debugPrivilege) { [Diagnostics.Process]::LeaveDebugMode() }
    }
}

function Wait-IndexerStopped([int]$Seconds) {
    $controller = Get-Service -Name $script:IndexerServiceName -ErrorAction SilentlyContinue
    if (-not $controller) { return }
    try {
        $controller.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds($Seconds))
    } finally { $controller.Dispose() }
}

function Invoke-IndexerSc([string[]]$Arguments, [int[]]$AllowedExitCodes = @(0)) {
    $options = New-Object Diagnostics.ProcessStartInfo
    $options.FileName = "$env:SystemRoot\System32\sc.exe"
    $options.Arguments = ($Arguments | ForEach-Object { '"' + $_ + '"' }) -join ' '
    $options.UseShellExecute = $false
    $options.CreateNoWindow = $true
    $options.RedirectStandardOutput = $true
    $options.RedirectStandardError = $true
    $command = [Diagnostics.Process]::Start($options)
    try {
        $output = $command.StandardOutput.ReadToEndAsync()
        $errors = $command.StandardError.ReadToEndAsync()
        if (-not $command.WaitForExit(3000)) {
            $command.Kill()
            $command.WaitForExit()
            throw "sc.exe timed out: $Arguments"
        }
        Write-Output $output.GetAwaiter().GetResult()
        $stderr = $errors.GetAwaiter().GetResult()
        if ($stderr) { Write-Output $stderr }
        if ($command.ExitCode -notin $AllowedExitCodes) { throw "sc.exe failed ($($command.ExitCode)): $Arguments" }
    } finally { $command.Dispose() }
}

function Restore-IndexerStartup([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return }
    $saved = Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
    if (-not $saved.exists -or -not (Get-IndexerService)) { return }
    $startType = switch ($saved.startMode) {
        'Auto' { if ($saved.delayedAutoStart) { 'delayed-auto' } else { 'auto' } }
        'Manual' { 'demand' }
        'Disabled' { 'disabled' }
        default { throw 'Invalid saved indexer startup mode' }
    }
    Write-Output 'Restoring the previous indexer startup configuration'
    Invoke-IndexerSc -Arguments @('config', $script:IndexerServiceName, 'start=', $startType)
    if ($saved.wasRunning) { Invoke-IndexerSc -Arguments @('start', $script:IndexerServiceName) -AllowedExitCodes @(0, 1056) }
}

function Get-IndexerDataDirectory {
    Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'com.everything.app\data'
}

function Backup-StoppedIndexerDatabase([string]$Path) {
    $dataRoot = Get-IndexerDataDirectory
    $database = Join-Path $dataRoot 'index.db'
    if (-not (Test-Path -LiteralPath $database -PathType Leaf)) { return }
    $service = Get-IndexerService
    if ($service -and ($service.State -ne 'Stopped' -or $service.ProcessId -ne 0)) {
        throw 'Refusing to copy the index while the indexer is running'
    }
    $backupRoot = Join-Path $dataRoot 'upgrade-backups'
    $backup = Join-Path $backupRoot ((Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $backup | Out-Null
    foreach ($name in @('index.db', 'index.db-wal', 'index.db-shm')) {
        $source = Join-Path $dataRoot $name
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Write-Output "Backing up $name after all installed processes have stopped"
            Copy-Item -LiteralPath $source -Destination (Join-Path $backup $name) -ErrorAction Stop
        }
    }
    $saved = Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
    $saved | Add-Member -NotePropertyName backupDirectory -NotePropertyValue $backup -Force
    $saved | ConvertTo-Json | Set-Content -LiteralPath $Path -Encoding UTF8
    Write-Output "Preserved index.db, WAL and SHM in $backup"
}

function Prepare-IndexerUpgrade([string]$Directory, [string]$Path) {
    if (-not $Directory -or -not $Path) { throw 'InstallDir and StateFile are required' }
    $installedBinary = Get-NormalizedExecutablePath (Join-Path $Directory 'Everything.exe')
    $service = Get-IndexerService
    $registeredBinary = if ($service) { Get-IndexerExecutablePath $service.PathName } else { $null }
    $saved = if ($service) {
        @{ exists = $true; startMode = $service.StartMode; delayedAutoStart = [bool]$service.DelayedAutoStart;
           wasRunning = $service.State -ne 'Stopped' }
    } else { @{ exists = $false } }
    $saved | ConvertTo-Json | Set-Content -LiteralPath $Path -Encoding UTF8
    try {
        if ($service) {
            # Prevent both SCM recovery and GUI clients from restarting the old
            # binary between termination and file replacement. POSTINSTALL sets
            # delayed-auto again; failure/abort restores the saved setting.
            Write-Output 'Temporarily disabling indexer restarts during installation'
            Set-Service -Name $script:IndexerServiceName -StartupType Disabled -ErrorAction Stop
            if ($service.State -ne 'Stopped') {
                Write-Output 'Requesting a graceful stop (up to 5 seconds)'
                try {
                    Invoke-IndexerSc -Arguments @('stop', $script:IndexerServiceName) -AllowedExitCodes @(0, 1062, 1061)
                    Wait-IndexerStopped 5
                } catch {
                    Write-Output "Graceful stop did not finish: $_"
                    $live = Get-IndexerService
                    if ($live.ProcessId -gt 0) { Stop-ValidatedUpgradeProcess $live.ProcessId $registeredBinary }
                }
                Wait-IndexerStopped 10
            }
        }
        # WMI supplies paths for x64 clients even from a 32-bit installer.
        # Match exact installed/registered binaries, not a process name alone.
        Write-Output 'Closing installed Everything clients before file replacement'
        foreach ($client in @(Get-CimInstance Win32_Process -Filter "Name='Everything.exe'" -ErrorAction Stop)) {
            if (-not $client.ExecutablePath) { continue }
            $clientPath = Get-NormalizedExecutablePath $client.ExecutablePath
            if ([string]::Equals($clientPath, $installedBinary, [StringComparison]::OrdinalIgnoreCase) -or
                ($registeredBinary -and [string]::Equals($clientPath, $registeredBinary, [StringComparison]::OrdinalIgnoreCase))) {
                Stop-ValidatedUpgradeProcess $client.ProcessId $clientPath
            }
        }
        Backup-StoppedIndexerDatabase $Path
    } catch {
        $failure = $_
        try { Restore-IndexerStartup $Path } catch { Write-Warning "Could not restore indexer startup: $_" }
        throw $failure
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    try {
        if ($Mode -eq 'Restore') { Restore-IndexerStartup $StateFile }
        else { Prepare-IndexerUpgrade $InstallDir $StateFile }
    } catch {
        Write-Error $_ -ErrorAction Continue
        exit 1
    }
}
