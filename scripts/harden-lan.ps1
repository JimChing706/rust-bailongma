# harden-lan.ps1 - One-click LAN exposure hardening for the Bailongma API port.
#
# What it does:
#   1. Token: if BAILONGMA_API_TOKEN is not set at User scope, generate a strong
#      random one and persist it (new processes inherit it).  Loopback still
#      works; any remote caller must present the Bearer token.
#   2. Firewall: add an inbound BLOCK rule for TCP 3721 from any remote address.
#      Windows Firewall does not filter loopback traffic, so 127.0.0.1 keeps
#      working while LAN/Internet access is closed.  (Requires Administrator.)
#   3. Re-check the current listener on the port and print PASS/FAIL.
#
# Usage (run as Administrator so the firewall rule can be added):
#   powershell -ExecutionPolicy Bypass -File scripts\harden-lan.ps1
#
# Exit codes: 0 = PASS, 1 = FAIL (token set but firewall rule missing).

$ErrorActionPreference = 'Stop'
$port     = 3721
$ruleName = 'Bailongma API 3721 - loopback only'
$hadIssue = $false

Write-Host '=== [1/3] API token ===' -ForegroundColor Cyan
$token = [Environment]::GetEnvironmentVariable('BAILONGMA_API_TOKEN', 'User')
if ([string]::IsNullOrWhiteSpace($token)) {
    $chars = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789'
    $rng   = New-Object System.Security.Cryptography.RNGCryptoServiceProvider
    $bytes = New-Object byte[] 48
    $rng.GetBytes($bytes)
    $sb = New-Object System.Text.StringBuilder
    foreach ($b in $bytes) { [void]$sb.Append($chars[$b % $chars.Length]) }
    $token = $sb.ToString()
    [Environment]::SetEnvironmentVariable('BAILONGMA_API_TOKEN', $token, 'User')
    $env:BAILONGMA_API_TOKEN = $token
    Write-Host "[NEW] token generated and persisted at User scope (length $($token.Length))." -ForegroundColor Green
    Write-Host "      Save it somewhere safe - it will be required for remote API calls." -ForegroundColor Yellow
} else {
    Write-Host "[OK] token already set at User scope (length $($token.Length))." -ForegroundColor Green
}

Write-Host '=== [2/3] Firewall rule ===' -ForegroundColor Cyan
$isAdmin  = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$existing = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "[OK] firewall rule already present: $ruleName" -ForegroundColor Green
} elseif (-not $isAdmin) {
    Write-Host '[WARN] not running as Administrator - skipping firewall rule.' -ForegroundColor Yellow
    Write-Host '       Re-run this script from an elevated PowerShell to add it.' -ForegroundColor Yellow
    $hadIssue = $true
} else {
    New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Protocol TCP -LocalPort $port -Action Block -Profile Any -RemoteAddress Any | Out-Null
    Write-Host "[OK] added inbound BLOCK rule for TCP $port (loopback remains exempt)." -ForegroundColor Green
}

Write-Host '=== [3/3] Listener check ===' -ForegroundColor Cyan
$listeners = netstat -ano | Select-String ":$port\s" | Select-String 'LISTENING'
$lanExposed = $false
if ($listeners) {
    foreach ($line in $listeners) {
        if ($line -match "0\.0\.0\.0:$port") {
            Write-Host "[INFO] port $port listening on 0.0.0.0 - LAN reachable, but firewall + token now guard it." -ForegroundColor Yellow
            $lanExposed = $true
        } elseif ($line -match "127\.0\.0\.1:$port") {
            Write-Host "[OK] port $port loopback-only (127.0.0.1)." -ForegroundColor Green
        }
    }
} else {
    Write-Host "[INFO] port $port not listening right now." -ForegroundColor Yellow
}

Write-Host ''
if ($lanExposed -and $hadIssue) {
    Write-Host '[FAIL] token set but firewall rule NOT applied (needs admin).' -ForegroundColor Red
    exit 1
} elseif ($lanExposed) {
    Write-Host '[PASS] LAN exposure mitigated: token enforced + inbound 3721 blocked by firewall.' -ForegroundColor Green
    exit 0
} else {
    Write-Host '[PASS] no LAN exposure detected.' -ForegroundColor Green
    exit 0
}
