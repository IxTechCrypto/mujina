# Start both the Mujina miner daemon and its web dashboard server in one script.
# Usage: .\start-all.ps1

$here = $PSScriptRoot

Write-Host "=== Mujina Rig Bootstrapping (Windows) ===" -ForegroundColor Cyan

# 1. Start miner daemon
& "$here\start-mujina.ps1"
Start-Sleep -Seconds 2

# 2. Start web dashboard server
$pyProcs = Get-CimInstance Win32_Process -Filter "Name = 'python.exe' or Name = 'python3.exe'" -ErrorAction SilentlyContinue | Where-Object { $_.CommandLine -like "*dashboard.py*" }

if ($pyProcs) {
    Write-Host "Web dashboard server is already running." -ForegroundColor Yellow
} else {
    Write-Host "Starting Web Dashboard Server..." -ForegroundColor Green
    $dashProc = Start-Process -FilePath "python" -ArgumentList "`"$here\dashboard.py`"" -WorkingDirectory $here -PassThru
    Write-Host "Dashboard started in background (PID $($dashProc.Id))." -ForegroundColor Cyan
}

Write-Host "=================================" -ForegroundColor Cyan
Write-Host "All systems ready. Access dashboard at http://127.0.0.1:8088" -ForegroundColor Green
