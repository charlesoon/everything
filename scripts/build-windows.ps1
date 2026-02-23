$ErrorActionPreference = "Stop"

Write-Host "=== Everything Windows Build ===" -ForegroundColor Cyan

function Exit-IfFailed {
    param(
        [Parameter(Mandatory = $true)][string]$StepName
    )

    if ($LASTEXITCODE -ne 0) {
        Write-Host "$StepName failed (exit code: $LASTEXITCODE)" -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

# Check required tools
$missing = @()
if (-not (Get-Command node -ErrorAction SilentlyContinue)) { $missing += "Node.js" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { $missing += "Rust/cargo" }
if (-not (Get-Command npm -ErrorAction SilentlyContinue)) { $missing += "npm" }

if ($missing.Count -gt 0) {
    Write-Host "Missing required tools: $($missing -join ', ')" -ForegroundColor Red
    exit 1
}

Write-Host "Node.js: $(node --version)" -ForegroundColor Gray
Write-Host "Rust: $(rustc --version)" -ForegroundColor Gray
Write-Host "npm: $(npm --version)" -ForegroundColor Gray

# Force npm to resolve Windows optional dependencies for this Windows build script.
$env:npm_config_os = "win32"

# Install frontend dependencies
Write-Host "`nInstalling frontend dependencies..." -ForegroundColor Yellow
npm ci
Exit-IfFailed "npm ci"

# Work around npm optional dependency bug that can skip Tauri's native CLI binding on Windows.
$tauriCliNativePkg = "node_modules\@tauri-apps\cli-win32-x64-msvc"
if (-not (Test-Path $tauriCliNativePkg)) {
    $tauriCliNativeVersion = $null
    if (Test-Path "package.json") {
        $pkg = Get-Content "package.json" -Raw | ConvertFrom-Json
        if ($pkg.optionalDependencies) {
            $tauriCliNativeVersion = $pkg.optionalDependencies.'@tauri-apps/cli-win32-x64-msvc'
        }
    }

    if (-not $tauriCliNativeVersion) {
        Write-Host "Missing Tauri Windows native CLI binding and version lookup failed." -ForegroundColor Red
        Write-Host "Try: npm i @tauri-apps/cli-win32-x64-msvc --save-optional" -ForegroundColor Yellow
        exit 1
    }

    Write-Host "Tauri native CLI binding is missing (npm optional dependency bug). Installing fallback..." -ForegroundColor Yellow
    npm i --no-save "@tauri-apps/cli-win32-x64-msvc@$tauriCliNativeVersion"
    Exit-IfFailed "npm install fallback @tauri-apps/cli-win32-x64-msvc"

    if (-not (Test-Path $tauriCliNativePkg)) {
        Write-Host "Fallback install completed but the native CLI package is still missing." -ForegroundColor Red
        Write-Host "Try running manually:" -ForegroundColor Yellow
        Write-Host "  npm i --no-save @tauri-apps/cli-win32-x64-msvc@$tauriCliNativeVersion" -ForegroundColor Yellow
        Write-Host "If it still fails, delete node_modules and run npm install (not npm ci) once." -ForegroundColor Yellow
        exit 1
    }
}

# Build Tauri app with NSIS installer
Write-Host "`nBuilding Tauri app (NSIS)..." -ForegroundColor Yellow
npx tauri build --bundles nsis
Exit-IfFailed "tauri build --bundles nsis"

# Output result
$nsisDir = "src-tauri\target\release\bundle\nsis"
if (Test-Path $nsisDir) {
    Write-Host "`nBuild complete! Installer:" -ForegroundColor Green
    Get-ChildItem $nsisDir -Filter "*.exe" | ForEach-Object {
        Write-Host "  $($_.FullName)" -ForegroundColor Green
    }
} else {
    Write-Host "`nBuild failed: $nsisDir not found" -ForegroundColor Red
    exit 1
}
