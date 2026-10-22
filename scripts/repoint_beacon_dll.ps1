# 把已注册的 TSF 光标信标 CLSID 指向 clipx 仓库里的新 DLL。
# 需要管理员（HKLM 写权限）。供开发期切换 DLL 用。
# 用法（管理员 PowerShell）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\repoint_beacon_dll.ps1
$ErrorActionPreference = "Stop"

$tip = "{C4BE0C0A-9326-4C1A-BEAC-0CBEAC0DEC01}"
$repo = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$dll = Join-Path $repo "target\release\clipx_caret_beacon.dll"

if (-not (Test-Path $dll)) {
    throw "找不到 $dll，先跑 cargo build -p clipx-tsf --release"
}

$key = "HKLM:\SOFTWARE\Classes\CLSID\$tip\InProcServer32"
Write-Output "old: $((Get-ItemProperty $key).'(default)')"
Set-ItemProperty -Path $key -Name "(default)" -Value $dll
Set-ItemProperty -Path $key -Name "ThreadingModel" -Value "Apartment"
Write-Output "new: $((Get-ItemProperty $key).'(default)')"
Write-Output "done"
