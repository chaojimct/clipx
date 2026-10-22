# 生成 clipx 光标信标的代码签名证书（自签）+ 签名 DLL。
#
# TSF 对第三方 TIP 有强制要求：必须数字签名，否则拒绝加载。
# 这里用自签证书（CN=clipx Caret Beacon），并把证书导出到 scripts\clipx_beacon_cert.cer，
# 由 register_beacon_hklm.bat 导入机器级 Root + TrustedPublisher。
#
# 注意：自签证书只适合内部/自用分发。对外正式发布需要换成受信任 CA 的代码签名证书，
# 否则其他用户机器上无法安装。
#
# 幂等：证书已存在则复用。

$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptDir
$cerPath = Join-Path $scriptDir "clipx_beacon_cert.cer"
$dllPath = Join-Path $repoRoot "target\release\clipx_caret_beacon.dll"

$subject = "CN=clipx Caret Beacon"

Write-Output "=== clipx beacon cert ==="
Write-Output "subject : $subject"
Write-Output "cer     : $cerPath"
Write-Output "dll     : $dllPath"

# --- 1. 取得或创建证书（机器级 My 存储，因为注册脚本是机器级的） ---
$existing = Get-ChildItem Cert:\LocalMachine\My -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq $subject }

if ($existing) {
    $cert = $existing[0]
    Write-Output "reuse cert thumbprint=$($cert.Thumbprint)"
} else {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject `
        -CertStoreLocation Cert:\LocalMachine\My -KeyUsage DigitalSignature `
        -NotAfter (Get-Date).AddYears(3)
    Write-Output "created cert thumbprint=$($cert.Thumbprint)"
}

# --- 2. 导出公钥证书 ---
Export-Certificate -Cert $cert -FilePath $cerPath | Out-Null
Write-Output "exported cer -> $cerPath"

# --- 3. 签名 DLL（存在才签） ---
if (Test-Path $dllPath) {
    $r = Set-AuthenticodeSignature -FilePath $dllPath -Certificate $cert
    Write-Output "sigstatus=$($r.Status) $($r.StatusMessage)"
    if ($r.Status -ne "Valid") {
        Write-Output "WARNING: 签名状态非 Valid"
    }
} else {
    Write-Output "skip signing: dll not found ($dllPath)"
    Write-Output "先跑: cargo build -p clipx-tsf --release"
}

Write-Output "done"
