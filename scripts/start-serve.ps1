# Start the bailongma API server in the background and wait for /status to be ready.
#
# Usage:
#   .\scripts\start-serve.ps1            # debug build
#   .\scripts\start-serve.ps1 -Release   # release build
#   $env:BAILONGMA_API_TOKEN="secret"; .\scripts\start-serve.ps1   # enable token auth
#
# Stop: .\scripts\stop-serve.ps1
param(
    [switch]$Release
)
$ErrorActionPreference = 'Stop'

$root   = Split-Path -Parent $PSScriptRoot          # parent of scripts/ = workspace root
$logDir = Join-Path $root 'logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$logFile = Join-Path $logDir 'serve.log'
$errFile = Join-Path $logDir 'serve.log.err'
$pidFile = Join-Path $logDir 'serve.pid'
$port    = 3721

# 1) Build first (ensure latest code)
$cfgArgs = @('build')
if ($Release) { $cfgArgs += '--release' }
$cfgArgs += @('-p', 'bailongma-app', '--bin', 'serve')
Write-Host "[1/3] cargo $($cfgArgs -join ' ') ..."
Push-Location $root
try {
    & cargo @cfgArgs
    if ($LASTEXITCODE -ne 0) { throw "Build failed (exit=$LASTEXITCODE)" }
} finally {
    Pop-Location
}

# 2) Launch (background + output redirection + record PID)
$mode = if ($Release) { 'release' } else { 'debug' }
$exe  = Join-Path $root "target\$mode\serve.exe"
if (-not (Test-Path $exe)) { throw "Not found: $exe" }

# Stop a leftover instance first (PID file may be stale; also check port + name)
$toStop = @{}
if (Test-Path $pidFile) {
    Get-Content $pidFile -ErrorAction SilentlyContinue |
        ForEach-Object { $_.Trim() } | Where-Object { $_ } | ForEach-Object { $toStop[$_] = $true }
}
Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue |
    ForEach-Object { $toStop[$_.OwningProcess] = $true }
Get-Process -Name 'serve' -ErrorAction SilentlyContinue |
    ForEach-Object { $toStop[$_.Id] = $true }
foreach ($id in $toStop.Keys) {
    $old = Get-Process -Id $id -ErrorAction SilentlyContinue
    if ($old -and $old.ProcessName -eq 'serve') {
        Stop-Process -Id $id -Force
        Write-Host "[*] Stopped old instance PID=$id"
    }
}
Remove-Item $pidFile -Force -ErrorAction SilentlyContinue

Write-Host "[2/3] Starting $exe"
# NOTE: use -WindowStyle Hidden instead of -NoNewWindow. Under PowerShell 5.1 the
# -NoNewWindow + -RedirectStandardOutput combo can leak the parent's console pipe
# handles into the child, which keeps callers (CI, agent shells) blocked on EOF
# and can make the child get killed with the caller's process tree.
$p = Start-Process -FilePath $exe `
    -WorkingDirectory $root `
    -WindowStyle Hidden `
    -RedirectStandardOutput $logFile `
    -RedirectStandardError $errFile `
    -PassThru
$p.Id | Set-Content -Path $pidFile

# 3) Poll until ready (agent scan may take up to 15s; give margin)
Write-Host "[3/3] Waiting for http://127.0.0.1:$port/status ..."
$deadline = (Get-Date).AddSeconds(45)
$ready = $false
while ((Get-Date) -lt $deadline) {
    if ($p.HasExited) { throw "serve exited (code=$($p.ExitCode)); see $logFile / $errFile" }
    try {
        $r = Invoke-RestMethod -Uri "http://127.0.0.1:$port/status" -TimeoutSec 2
        if ($r.running) { $ready = $true; break }
    } catch {
        Start-Sleep -Milliseconds 500
    }
}
if (-not $ready) { throw "Timed out waiting for /status; see $logFile / $errFile" }

# Rewrite PID file with the real listener PID (Start-Process PID may be an intermediate)
$realPid = Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty OwningProcess
if ($realPid) {
    $realPid | Set-Content -Path $pidFile
    $shown = $realPid
} else {
    $shown = $p.Id
}

Write-Host ""
Write-Host "OK: serve is ready. PID=$shown  port=$port"
Write-Host "  Log:  $logFile"
Write-Host "  Stop: .\scripts\stop-serve.ps1"
Write-Host "  Check: curl http://127.0.0.1:$port/status"
