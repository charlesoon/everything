$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path $PSScriptRoot -Parent
. (Join-Path $repoRoot 'app/windows/prepare-upgrade.ps1')
$realStopProcess = ${function:Stop-ValidatedUpgradeProcess}
$realBackup = ${function:Backup-StoppedIndexerDatabase}
$fixtureParent = [IO.Path]::GetFullPath((Join-Path $repoRoot '.artifacts/windows-installer-tests'))
$fixtureRoot = Join-Path $fixtureParent ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixtureRoot | Out-Null
Set-Content -LiteralPath (Join-Path $fixtureRoot '.installer-fixture') -Value 'isolated installer tests'
$script:fixtureData = Join-Path $fixtureRoot 'data'
$script:caseCount = 0

function Assert-Installer($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function New-InstallerModel {
    @{
        Service = [pscustomobject]@{ State='Running'; ProcessId=100; StartMode='Auto'; DelayedAutoStart=$true; PathName='"C:\Program Files\Everything\Everything.exe" --service' }
        Events = New-Object 'Collections.Generic.List[string]'
        Clients = @(
            [pscustomobject]@{ ProcessId=200; ExecutablePath='C:\Program Files\Everything\Everything.exe' },
            [pscustomobject]@{ ProcessId=300; ExecutablePath='C:\Other\Everything.exe' }
        )
        Hung = $false; KillFails = $false; SurvivesKill = $false; RealProcess = $false
    }
}

# All service operations are simulated. Only the owned worker process in the
# last case is really terminated; the installed service is never changed.
function Get-IndexerService { $script:model.Service }
function Set-Service {
    param($Name, $StartupType)
    Assert-Installer ($Name -eq 'EverythingIndexer' -and $StartupType -eq 'Disabled') 'Unexpected service mutation'
    $script:model.Events.Add('disable')
}
function Invoke-IndexerSc {
    param([string[]]$Arguments, [int[]]$AllowedExitCodes)
    $script:model.Events.Add(('sc:' + ($Arguments -join ' ')))
    if ($Arguments[0] -eq 'stop' -and -not $script:model.Hung) {
        $script:model.Service.State = 'Stopped'
        $script:model.Service.ProcessId = 0
    }
}
function Wait-IndexerStopped {
    param([int]$Seconds)
    $script:model.Events.Add("wait:$Seconds")
    if ($script:model.Service.State -ne 'Stopped') { throw 'simulated stop timeout' }
}
function Stop-ValidatedUpgradeProcess {
    param([uint32]$TargetProcessId, [string]$ExpectedPath)
    $script:model.Events.Add("kill:$TargetProcessId")
    Assert-Installer ($ExpectedPath -eq 'C:\Program Files\Everything\Everything.exe') 'Unrelated executable selected'
    if ($script:model.KillFails) { throw 'simulated access denied' }
    if ($TargetProcessId -eq 100 -and -not $script:model.SurvivesKill) {
        $script:model.Service.State = 'Stopped'
        $script:model.Service.ProcessId = 0
    }
}
function Get-CimInstance {
    param($ClassName, $Filter)
    if ($script:model.RealProcess) { return CimCmdlets\Get-CimInstance -ClassName $ClassName -Filter $Filter }
    Assert-Installer ($ClassName -eq 'Win32_Process' -and $Filter -eq "Name='Everything.exe'") 'Unexpected global process query'
    $script:model.Clients
}
function Backup-StoppedIndexerDatabase {
    param([string]$Path)
    if ($script:model.Service) { Assert-Installer ($script:model.Service.State -eq 'Stopped') 'Backup ran while writer was alive' }
    $script:model.Events.Add('backup')
}
function Get-IndexerDataDirectory { $script:fixtureData }

function Invoke-InstallerCase([string]$Name, [scriptblock]$Case) {
    $script:model = New-InstallerModel
    $script:testState = Join-Path $fixtureRoot ([guid]::NewGuid().ToString('N') + '.json')
    & $Case
    $script:caseCount++
    Write-Output "PASS: $Name"
}

try {
    Invoke-InstallerCase 'first install does not stop a nonexistent service or unrelated app' {
        $script:model.Service = $null
        $script:model.Clients = @([pscustomobject]@{ ProcessId=300; ExecutablePath='C:\Other\Everything.exe' })
        Prepare-IndexerUpgrade 'C:\Program Files\Everything' $testState
        Assert-Installer ($model.Events.Count -eq 1 -and $model.Events[0] -eq 'backup') 'First install mutated service/processes'
    }
    Invoke-InstallerCase 'normal stop drains before closing installed clients and backing up' {
        Prepare-IndexerUpgrade 'C:\Program Files\Everything' $testState
        Assert-Installer ($model.Events[0] -eq 'disable') 'Restarts must be disabled first'
        Assert-Installer ($model.Events -contains 'wait:5' -and $model.Events -notcontains 'kill:100') 'Healthy writer was force-killed'
        Assert-Installer ($model.Events -contains 'kill:200' -and $model.Events -notcontains 'kill:300') 'Client path scoping failed'
        Assert-Installer ($model.Events[$model.Events.Count-1] -eq 'backup') 'Backup must follow all closes'
    }
    Invoke-InstallerCase 'hung writer is force-stopped by its registered PID' {
        $model.Hung = $true
        Prepare-IndexerUpgrade 'C:\Program Files\Everything' $testState
        Assert-Installer ($model.Events -contains 'kill:100' -and $model.Events -contains 'wait:10') 'Missing forced-stop fallback'
        $saved = Get-Content -LiteralPath $testState -Raw | ConvertFrom-Json
        Assert-Installer ($saved.startMode -eq 'Auto' -and $saved.delayedAutoStart -and $saved.wasRunning) 'Startup snapshot lost'
    }
    Invoke-InstallerCase 'failed termination restores startup and prevents replacement/backup' {
        $model.Hung = $true; $model.KillFails = $true
        $failed = $false
        try { Prepare-IndexerUpgrade 'C:\Program Files\Everything' $testState } catch { $failed = $true }
        Assert-Installer ($failed -and $model.Events -notcontains 'backup') 'Failure was ignored'
        Assert-Installer ($model.Events -contains 'sc:config EverythingIndexer start= delayed-auto') 'Startup was not restored'
        Assert-Installer ($model.Events -contains 'sc:start EverythingIndexer') 'Previously running service was not restarted'
    }
    Invoke-InstallerCase 'installer refuses replacement if service remains alive after kill' {
        $model.Hung = $true; $model.SurvivesKill = $true
        $failed = $false
        try { Prepare-IndexerUpgrade 'C:\Program Files\Everything' $testState } catch { $failed = $true }
        Assert-Installer ($failed -and $model.Events -notcontains 'backup') 'Live writer was ignored'
    }
    Invoke-InstallerCase 'failure restores an originally disabled service without starting it' {
        @{ exists=$true; startMode='Disabled'; delayedAutoStart=$false; wasRunning=$false } | ConvertTo-Json | Set-Content -LiteralPath $testState
        Restore-IndexerStartup $testState
        Assert-Installer ($model.Events.Count -eq 1 -and $model.Events[0] -eq 'sc:config EverythingIndexer start= disabled') 'Disabled service setting changed'
    }
    Invoke-InstallerCase 'unexpected service image aborts before disabling or terminating anything' {
        $model.Service.PathName = '"C:\Other\Unrelated.exe" --service'
        $failed = $false
        try { Prepare-IndexerUpgrade 'C:\Program Files\Everything' $testState } catch { $failed = $true }
        Assert-Installer ($failed -and $model.Events.Count -eq 0) 'Unexpected service image was modified'
    }
    Invoke-InstallerCase 'stopped database, WAL and SHM are copied byte-for-byte without deleting originals' {
        $model.Service.State='Stopped'; $model.Service.ProcessId=0
        New-Item -ItemType Directory -Path $fixtureData | Out-Null
        foreach ($name in @('index.db', 'index.db-wal', 'index.db-shm')) {
            [IO.File]::WriteAllBytes((Join-Path $fixtureData $name), [byte[]](0, 1, 2, 255, 42))
        }
        @{ exists=$true; startMode='Auto'; wasRunning=$true } | ConvertTo-Json | Set-Content -LiteralPath $testState
        & $realBackup $testState
        $saved = Get-Content -LiteralPath $testState -Raw | ConvertFrom-Json
        Assert-Installer ($saved.backupDirectory.StartsWith($fixtureRoot + '\')) 'Backup escaped fixture'
        foreach ($name in @('index.db', 'index.db-wal', 'index.db-shm')) {
            Assert-Installer ((Get-FileHash -LiteralPath (Join-Path $fixtureData $name)).Hash -eq (Get-FileHash -LiteralPath (Join-Path $saved.backupDirectory $name)).Hash) "Backup differs: $name"
        }
    }
    Invoke-InstallerCase 'real forced stop validates image, refuses mismatch, and handles an owned x64 process' {
        $model.RealProcess = $true
        $expected = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $launch = if (-not [Environment]::Is64BitProcess -and [Environment]::Is64BitOperatingSystem) {
            Join-Path $env:SystemRoot 'Sysnative\WindowsPowerShell\v1.0\powershell.exe'
        } else { $expected }
        $worker = Start-Process -FilePath $launch -ArgumentList '-NoProfile -NonInteractive -Command "Start-Sleep -Seconds 30"' -WindowStyle Hidden -PassThru
        try {
            $refused = $false
            try { & $realStopProcess $worker.Id 'C:\Other\Unrelated.exe' } catch { $refused = $true }
            Assert-Installer ($refused -and -not $worker.HasExited) 'Wrong image was not protected'
            & $realStopProcess $worker.Id $expected
            Assert-Installer ($worker.WaitForExit(1000)) 'Owned worker did not stop'
        } finally {
            if (-not $worker.HasExited) { $worker.Kill(); $worker.WaitForExit() }
            $worker.Dispose()
        }
    }
    Write-Output "Installer checks passed: $caseCount (host x64=$([Environment]::Is64BitProcess))"
} finally {
    $resolved = [IO.Path]::GetFullPath($fixtureRoot)
    if (-not $resolved.StartsWith($fixtureParent + '\', [StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath (Join-Path $resolved '.installer-fixture'))) { throw 'Unsafe fixture cleanup path' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
