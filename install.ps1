# Raincode installer (Windows PowerShell)
# Usage: from the repo root, run:
#   powershell -ExecutionPolicy Bypass -File install.ps1
# What it does: builds the release binary, copies raincode.exe to
# ~/.raincode/bin, and adds that dir to the user PATH so you can type
# `raincode` in any terminal.

$ErrorActionPreference = "Stop"

$Repo = $PSScriptRoot
$Bin = Join-Path $HOME ".raincode\bin"
$Exe = Join-Path $Repo "target\release\raincode.exe"

# 1) Ensure the release binary exists (build if missing)
if (-not (Test-Path $Exe)) {
    Write-Host ">>> Building release (first time takes a while)..." -ForegroundColor Cyan
    Push-Location $Repo
    cargo build --release
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "cargo build --release failed" }
    Pop-Location
    if (-not (Test-Path $Exe)) { throw "Build finished but $Exe not found" }
}

# 2) Copy to ~/.raincode/bin
New-Item -ItemType Directory -Force -Path $Bin | Out-Null
Copy-Item $Exe (Join-Path $Bin "raincode.exe") -Force

# 3) Add to user PATH (idempotent)
$Current = [Environment]::GetEnvironmentVariable("Path", "User")
if ($Current -and $Current.Contains($Bin)) {
    Write-Host "[ok] $Bin is already on PATH." -ForegroundColor Green
} else {
    [Environment]::SetEnvironmentVariable("Path", "$Current;$Bin", "User")
    Write-Host "[ok] Added $Bin to user PATH. Open a NEW terminal for it to take effect." -ForegroundColor Green
}

Write-Host ""
Write-Host "Raincode installed!" -ForegroundColor Green
Write-Host "  In a new terminal, from any folder:"
Write-Host "    raincode repl"                        # interactive TUI
Write-Host '    raincode run "write a hello world"'   # one-shot task
Write-Host "  First-time setup:"
Write-Host "    raincode setup"                       # configure model + API key
