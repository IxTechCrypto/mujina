param(
    [switch]$Foreground
)

# PowerShell script to start the Mujina miner daemon on Windows.
# Usage: .\start-mujina.ps1 [-Foreground]

$here = $PSScriptRoot
$candidates = @(
    (Join-Path $here "mujina-minerd.exe"),
    (Join-Path $here "target\release\mujina-minerd.exe"),
    (Join-Path $here "target\debug\mujina-minerd.exe")
) | Where-Object { Test-Path $_ } | Sort-Object { (Get-Item $_).LastWriteTime } -Descending

if ($candidates.Count -eq 0) {
    Write-Host "Error: mujina-minerd.exe binary not found at $here\mujina-minerd.exe or target\[release|debug]\mujina-minerd.exe" -ForegroundColor Red
    exit 1
}
$exe = $candidates[0]
Write-Host "Using binary: $exe (built $((Get-Item $exe).LastWriteTime))" -ForegroundColor DarkGray

# If already running, stop it gracefully first
$running = Get-Process -Name "mujina-minerd" -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "Mujina is already running. Stopping gracefully first..." -ForegroundColor Yellow
    & "$here\stop-mujina.ps1"
}

# --- API / state configuration ---
$env:MUJINA_API_LISTEN = "0.0.0.0:7785"
$env:RUST_LOG = "warn,mujina_miner=info"
$env:MUJINA_STATE_DIR = $here

# --- Pool Seed Configuration ---
$settings = Join-Path $here "mujina-settings.json"
if (-not (Test-Path $settings)) {
    $env:MUJINA_POOL_URL = "stratum+tcp://parasite.wtf:42069"
    $env:MUJINA_POOL_USER = "bc1qa70cqk8hl3jg6hgqlts66g2fqazpwspn3wrn80.Mujina_gamma"
    $env:MUJINA_POOL_PASS = "x"
    $poolDesc = "$env:MUJINA_POOL_URL (seeded from this script)"
} else {
    $poolDesc = "from $settings"
}

Write-Host "Starting Mujina miner daemon..." -ForegroundColor Green

if ($Foreground) {
    Write-Host "Running in foreground..." -ForegroundColor Cyan
    & $exe
} else {
    $proc = Start-Process -FilePath $exe -WorkingDirectory $here -PassThru
    Write-Host "Started Mujina (PID $($proc.Id))." -ForegroundColor Cyan
    Write-Host "  Pool: $poolDesc"
    Write-Host "  API : http://0.0.0.0:7785/api/v0/miner"
}
