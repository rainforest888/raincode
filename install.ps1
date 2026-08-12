# Raincode 安装脚本(Windows PowerShell)
# 用法:在仓库根执行  powershell -ExecutionPolicy Bypass -File install.ps1
# 效果:构建 release → 复制 raincode.exe 到 ~/.raincode/bin → 加入用户 PATH。
# 之后在任何文件夹的终端输入 `raincode` 即可打开。

$ErrorActionPreference = "Stop"

$Repo = $PSScriptRoot
$Bin = Join-Path $HOME ".raincode\bin"
$Exe = Join-Path $Repo "target\release\raincode.exe"

# 1) 确保 release 存在(没有就构建)
if (-not (Test-Path $Exe)) {
    Write-Host ">>> 构建 release(首次较慢)..." -ForegroundColor Cyan
    Push-Location $Repo
    cargo build --release
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "cargo build --release 失败" }
    Pop-Location
    if (-not (Test-Path $Exe)) { throw "构建成功但找不到 $Exe" }
}

# 2) 复制到 ~/.raincode/bin
New-Item -ItemType Directory -Force -Path $Bin | Out-Null
Copy-Item $Exe (Join-Path $Bin "raincode.exe") -Force

# 3) 加入用户 PATH(幂等)
$Current = [Environment]::GetEnvironmentVariable("Path", "User")
if ($Current -and $Current.Contains($Bin)) {
    Write-Host "✔ $Bin 已在 PATH 中。" -ForegroundColor Green
} else {
    [Environment]::SetEnvironmentVariable("Path", "$Current;$Bin", "User")
    Write-Host "✔ 已把 $Bin 加入用户 PATH(请新开一个终端生效)。" -ForegroundColor Green
}

Write-Host ""
Write-Host "Raincode 安装完成!" -ForegroundColor Green
Write-Host "  新终端里输入:  raincode repl    (交互式 TUI)"
Write-Host "               raincode run \"写个 hello world\"   (单次任务)"
Write-Host "  首次使用请先:  raincode setup   (配置模型 + API key)"
