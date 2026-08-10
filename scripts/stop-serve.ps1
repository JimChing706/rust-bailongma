# Stop the bailongma API server.
#
# Resolution order:
#   1. PID recorded in logs\serve.pid (may be stale / an intermediate PID)
#   2. Any process listening on port 3721
#   3. Any process named "serve" (fallback)
#
# Usage: .\scripts\stop-serve.ps1
$root    = Split-Path -Parent $PSScriptRoot
$pidFile = Join-Path $root 'logs\serve.pid'
$port    = 3721

$targets = @{}

# 1) PID file
if (Test-Path $pidFile) {
    $pidValue = Get-Content $pidFile -ErrorAction SilentlyContinue |
        ForEach-Object { $_.Trim() } | Where-Object { $_ }
    if ($pidValue) { $targets[$pidValue] = $true }
}

# 2) Listener on the API port
$conns = Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue
foreach ($c in $conns) { $targets[$c.OwningProcess] = $true }

# 3) Process-name fallback
Get-Process -Name 'serve' -ErrorAction SilentlyContinue |
    ForEach-Object { $targets[$_.Id] = $true }

if ($targets.Count -eq 0) {
    Write-Host 'No serve process found (nothing to stop)'
    Remove-Item $pidFile -Force -ErrorAction SilentlyContinue
    exit 0
}

$stopped = 0
foreach ($id in $targets.Keys) {
    $proc = Get-Process -Id $id -ErrorAction SilentlyContinue
    if ($proc -and $proc.ProcessName -eq 'serve') {
        Stop-Process -Id $id -Force
        Write-Host "Stopped serve (PID=$id)"
        $stopped++
    }
}
Remove-Item $pidFile -Force -ErrorAction SilentlyContinue

if ($stopped -eq 0) { Write-Host 'No live serve process matched (stale PIDs only)' }
