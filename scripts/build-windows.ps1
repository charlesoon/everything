param([switch]$SkipBuild)
$ErrorActionPreference = "Stop"
$repoRoot = Split-Path $PSScriptRoot -Parent
Push-Location $repoRoot
try {
    if (-not $SkipBuild) {
        & cargo build --release --locked --manifest-path app/Cargo.toml
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    }
    $makensis = (Get-Command makensis.exe -ErrorAction SilentlyContinue).Source
    if (-not $makensis) { $makensis = "${env:ProgramFiles(x86)}\NSIS\makensis.exe" }
    if (-not (Test-Path -LiteralPath $makensis)) {
        $makensis = Join-Path $env:LOCALAPPDATA "tauri\NSIS\makensis.exe"
    }
    if (-not (Test-Path $makensis)) { throw "Install NSIS 3, or add makensis.exe to PATH" }
    $version = ((Get-Content package.json -Raw) | ConvertFrom-Json).version
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "app\target" }
    $binary = (Resolve-Path "$targetRoot\release\everything.exe").Path
    $bundle = Join-Path $targetRoot "release\bundle\nsis"
    New-Item -ItemType Directory -Force -Path $bundle | Out-Null
    $output = Join-Path (Resolve-Path $bundle).Path "Everything_${version}_x64-setup.exe"
    & $makensis "/DVERSION=$version" "/DBINARY=$binary" "/DOUTPUT=$output" "/DICON=$repoRoot\app\icons\icon.ico" "$repoRoot\app\windows\installer.nsi"
    if ($LASTEXITCODE -ne 0) { throw "NSIS failed ($LASTEXITCODE)" }

    # Publish only after successful packaging. Hash the staged copy so the
    # update manifest describes the exact installer placed beside it.
    $releases = Join-Path $repoRoot "releases"
    New-Item -ItemType Directory -Force -Path $releases | Out-Null
    $installerName = [IO.Path]::GetFileName($output)
    $manifest = Join-Path $releases "latest-win.yml"
    $stagingId = [Guid]::NewGuid().ToString("N")
    $stagedInstaller = Join-Path $releases ".win-release-$stagingId.exe"
    $stagedManifest = Join-Path $releases ".latest-win-$stagingId.tmp"
    try {
        Copy-Item -LiteralPath $output -Destination $stagedInstaller
        $installerSize = (Get-Item -LiteralPath $stagedInstaller).Length
        $hasher = [Security.Cryptography.SHA512]::Create()
        $installerStream = [IO.File]::OpenRead($stagedInstaller)
        try {
            $sha512 = [Convert]::ToBase64String($hasher.ComputeHash($installerStream))
        } finally {
            $installerStream.Dispose()
            $hasher.Dispose()
        }
        $releaseDate = [DateTime]::UtcNow.ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", [Globalization.CultureInfo]::InvariantCulture)
        $metadata = @"
version: $version
files:
  - url: $installerName
    sha512: $sha512
    size: $installerSize
path: $installerName
sha512: $sha512
releaseDate: '$releaseDate'
"@
        [IO.File]::WriteAllText($stagedManifest, $metadata + "`n", [Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath $stagedInstaller -Destination (Join-Path $releases $installerName) -Force
        Move-Item -LiteralPath $stagedManifest -Destination $manifest -Force
    } finally {
        Remove-Item -LiteralPath $stagedInstaller, $stagedManifest -Force -ErrorAction SilentlyContinue
    }
    Write-Host $output
    Write-Host $manifest
} finally {
    Pop-Location
}
