param([switch]$SkipBuild)
$ErrorActionPreference = "Stop"
$repoRoot = Split-Path $PSScriptRoot -Parent
Push-Location $repoRoot
try {
    if (-not $SkipBuild) {
        & cargo build --release --locked --manifest-path src-tauri/Cargo.toml
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    }
    $makensis = (Get-Command makensis.exe -ErrorAction SilentlyContinue).Source
    if (-not $makensis) { $makensis = "${env:ProgramFiles(x86)}\NSIS\makensis.exe" }
    if (-not (Test-Path $makensis)) { throw "Install NSIS 3, or add makensis.exe to PATH" }
    $version = ((Get-Content package.json -Raw) | ConvertFrom-Json).version
    $targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "src-tauri\target" }
    $binary = (Resolve-Path "$targetRoot\release\everything.exe").Path
    $bundle = Join-Path $targetRoot "release\bundle\nsis"
    New-Item -ItemType Directory -Force -Path $bundle | Out-Null
    $output = Join-Path (Resolve-Path $bundle).Path "Everything_${version}_x64-setup.exe"
    & $makensis "/DVERSION=$version" "/DBINARY=$binary" "/DOUTPUT=$output" "/DICON=$repoRoot\src-tauri\icons\icon.ico" "$repoRoot\src-tauri\windows\installer.nsi"
    if ($LASTEXITCODE -ne 0) { throw "NSIS failed ($LASTEXITCODE)" }
    Write-Host $output
} finally {
    Pop-Location
}
