# ============================================================================
#  clipx 光标信标 DLL —— 双位数构建脚本
#
#  为什么要两个：
#    `SetWindowsHookEx` 要求「装钩子的进程」与「目标线程」同位数。clipx.exe 是
#    x64，装不进 32 位宿主（企业微信 WXWork、WPS、腾讯会议、WeMail…），所以
#    这些进程改由 clipx 远程注入 **x86 信标**，再由 DLL 在宿主内自装钩子。
#
#  产物：
#    target\release\clipx_caret_beacon.dll     x64（本机 64 位应用用）
#    target\release\clipx_caret_beacon32.dll   x86（32 位宿主用，文件名带 32）
#
#  用法：powershell -ExecutionPolicy Bypass -File scripts\build_beacon.ps1
# ============================================================================
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

Write-Host "[1/3] 构建 x64 信标..."
cargo build --release --locked -p clipx-tsf
if ($LASTEXITCODE -ne 0) { throw "x64 构建失败" }

Write-Host "[2/3] 构建 x86 信标（i686-pc-windows-msvc）..."
$targets = rustup target list --installed
if ($targets -notmatch "i686-pc-windows-msvc") {
    Write-Host "  安装 i686-pc-windows-msvc 目标..."
    rustup target add i686-pc-windows-msvc
}
cargo build --release --locked -p clipx-tsf --target i686-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "x86 构建失败" }

Write-Host "[3/3] 归集产物..."
$src = Join-Path $repo "target\i686-pc-windows-msvc\release\clipx_caret_beacon.dll"
$dst = Join-Path $repo "target\release\clipx_caret_beacon32.dll"
# 目标文件可能正被已注入的宿主进程占用（DLL 已映射且不可覆盖）。
# 改名再写是唯一可行路径：Windows 允许移动已映射 DLL 的目录项。
if (Test-Path $dst) {
    try {
        Copy-Item $src $dst -Force -ErrorAction Stop
    } catch {
        $bak = "$dst.bak"
        if (Test-Path $bak) { Remove-Item $bak -Force -ErrorAction SilentlyContinue }
        Rename-Item $dst (Split-Path -Leaf $bak) -Force
        Copy-Item $src $dst -Force
        Write-Host "  旧 DLL 被占用，已改名为 $(Split-Path -Leaf $bak)"
    }
} else {
    Copy-Item $src $dst -Force
}

foreach ($f in @("clipx_caret_beacon.dll", "clipx_caret_beacon32.dll")) {
    $p = Join-Path $repo "target\release\$f"
    if (-not (Test-Path $p)) { throw "缺少产物: $p" }
    $size = (Get-Item $p).Length
    Write-Host ("  OK {0,-30} {1,8:N0} bytes" -f $f, $size)
}
Write-Host "完成。"
