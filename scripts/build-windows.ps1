$ErrorActionPreference = "Stop"

Write-Host "=== Everything Windows Build ===" -ForegroundColor Cyan

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

# Install frontend dependencies
Write-Host "`nInstalling frontend dependencies..." -ForegroundColor Yellow
npm ci

# Build Tauri app with NSIS installer
Write-Host "`nBuilding Tauri app (NSIS)..." -ForegroundColor Yellow
npx tauri build --bundles nsis

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
