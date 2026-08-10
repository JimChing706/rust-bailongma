# LAN 暴露安全检查（检查项 #3）
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\check_lan_exposure.ps1
#
# 检查内容：
#   1. Bailongma API 端口（默认 3721）监听地址是否为 0.0.0.0（LAN 暴露）
#   2. BAILONGMA_API_TOKEN 是否已配置（LAN 开启时必须有 token）
#
# 判定：
#   PASS   仅回环监听，或 LAN 监听 + token 已配置
#   FAIL   LAN 监听但未配置 token —— 网内任意设备可直连 /message
#
# 修复（FAIL 时二选一）：
#   A. 配置 token：setx BAILONGMA_API_TOKEN "<强随机值>" 后重启 Bailongma
#   B. 关闭 LAN：config.json 中 network.allowLanAccess 改为 false，重启
#
# 退出码：0 = PASS，1 = FAIL

$ErrorActionPreference = "SilentlyContinue"
$port = 3721
$issues = @()

# 1) 监听地址检查
$listeners = netstat -ano | Select-String ":$port\s" | Select-String "LISTENING"
$lanExposed = $false
foreach ($line in $listeners) {
    if ($line -match "0\.0\.0\.0:$port") {
        $lanExposed = $true
        Write-Host "[FAIL] 端口 $port 监听在 0.0.0.0 —— LAN 暴露中" -ForegroundColor Red
    } elseif ($line -match "127\.0\.0\.1:$port") {
        Write-Host "[PASS] 端口 $port 仅回环监听（127.0.0.1）" -ForegroundColor Green
    }
}
if (-not $listeners) {
    Write-Host "[INFO] 端口 $port 未监听（服务未运行？）" -ForegroundColor Yellow
}

# 2) token 配置检查
$token = [Environment]::GetEnvironmentVariable("BAILONGMA_API_TOKEN", "User")
if ([string]::IsNullOrWhiteSpace($token)) {
    Write-Host "[WARN] 用户环境变量 BAILONGMA_API_TOKEN 未配置" -ForegroundColor Yellow
    $tokenSet = $false
} else {
    Write-Host "[PASS] BAILONGMA_API_TOKEN 已配置（长度 $($token.Length)）" -ForegroundColor Green
    $tokenSet = $true
}

# 3) 判定
if ($lanExposed -and -not $tokenSet) {
    Write-Host ""
    Write-Host "══════════════════════════════════════════════════" -ForegroundColor Red
    Write-Host "  FAIL: LAN 暴露 + 无 token —— 网内任意设备可直连 /message" -ForegroundColor Red
    Write-Host "  修复 A（推荐）：setx BAILONGMA_API_TOKEN \"<32位以上随机串>\"，重启 Bailongma" -ForegroundColor Yellow
    Write-Host "  修复 B：config.json 中 network.allowLanAccess 改为 false，重启" -ForegroundColor Yellow
    Write-Host "══════════════════════════════════════════════════" -ForegroundColor Red
    exit 1
} else {
    Write-Host ""
    Write-Host "[PASS] LAN 暴露检查通过" -ForegroundColor Green
    exit 0
}
