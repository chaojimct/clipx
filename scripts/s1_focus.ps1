$ErrorActionPreference = "Stop"
Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class S1Focus {
    public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder sb, int max);
}
'@
function Get-PopupVisible([int]$procId) {
    $script:visible = $false
    $null = [S1Focus]::EnumWindows([S1Focus+EnumWindowsProc]{
      param($h, $l)
      $wpid = 0; [S1Focus]::GetWindowThreadProcessId($h, [ref]$wpid) | Out-Null
      if ($wpid -eq $procId) {
        $cn = New-Object System.Text.StringBuilder 256; [S1Focus]::GetClassName($h, $cn, 256) | Out-Null
        if ($cn.ToString() -eq "Window Class" -and [S1Focus]::IsWindowVisible($h)) { $script:visible = $true }
      }
      return $true
    }, [IntPtr]::Zero)
    return $script:visible
}
function Get-FgInfo {
    $fg = [S1Focus]::GetForegroundWindow()
    $wt = New-Object System.Text.StringBuilder 256; [S1Focus]::GetWindowText($fg, $wt, 256) | Out-Null
    return "$fg|$($wt.ToString())"
}

$p = Start-Process -FilePath (Join-Path $PWD "target\release\clipx.exe") -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
$ws = New-Object -ComObject WScript.Shell

$fgBefore = Get-FgInfo
$ws.SendKeys("^%v")
Start-Sleep -Seconds 2
$fgAfter = Get-FgInfo
$visible = Get-PopupVisible $p.Id

Write-Host "foreground before: $fgBefore"
Write-Host "foreground after : $fgAfter"
Write-Host "popup visible    : $visible"
Write-Host ""
if (-not $visible) { Write-Host "FAIL: popup not shown"; Stop-Process -Id $p.Id -Force; exit 1 }
if ($fgBefore -ne $fgAfter) {
    Write-Host "=== S1 FAILED: focus stolen ==="
    Stop-Process -Id $p.Id -Force
    exit 1
}
Write-Host "=== S1 focus: PASSED (popup shows WITHOUT stealing focus) ==="

$ws.SendKeys("{ESC}")
Start-Sleep -Seconds 1
$visibleAfterEsc = Get-PopupVisible $p.Id
$fgAfterEsc = Get-FgInfo
Write-Host "after Esc: visible=$visibleAfterEsc foreground=$fgAfterEsc"
if ($visibleAfterEsc) { Write-Host "FAIL: Esc did not close popup"; Stop-Process -Id $p.Id -Force; exit 1 }
if ($p.HasExited) { Write-Host "FAIL: process exited"; exit 1 }
Write-Host "=== S1 Esc close: PASSED ==="
Stop-Process -Id $p.Id -Force
