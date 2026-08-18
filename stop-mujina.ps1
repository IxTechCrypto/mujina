# Gracefully stop the Mujina miner daemon on Windows.
# Usage: .\stop-mujina.ps1

$procs = Get-Process -Name "mujina-minerd" -ErrorAction SilentlyContinue

if (-not $procs) {
    Write-Host "Mujina is not running." -ForegroundColor Yellow
} else {
    foreach ($proc in $procs) {
        Write-Host "Stopping Mujina (PID $($proc.Id))..." -ForegroundColor Cyan
        $proc.CloseMainWindow() | Out-Null
        Start-Sleep -Seconds 1
        if (-not $proc.HasExited) {
            Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        }
    }
    Write-Host "Mujina stopped." -ForegroundColor Green
}

# --- Stop Web Dashboard Server if running ---
$pyProcs = Get-CimInstance Win32_Process -Filter "Name = 'python.exe' or Name = 'python3.exe'" -ErrorAction SilentlyContinue | Where-Object { $_.CommandLine -like "*dashboard.py*" }

foreach ($py in $pyProcs) {
    Write-Host "Stopping Web Dashboard Server (PID $($py.ProcessId))..." -ForegroundColor Cyan
    Stop-Process -Id $py.ProcessId -Force -ErrorAction SilentlyContinue
}
