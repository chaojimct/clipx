$ErrorActionPreference = "Stop"
$db = "target\release\Data\clipx.db"

function Copy-Text([string]$text) {
    for ($i = 0; $i -lt 10; $i++) {
        try { Set-Clipboard -Value $text; return $true } catch { Start-Sleep -Milliseconds 200 }
    }
    return $false
}

Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WinFinal {
    public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [StructLayout(LayoutKind.Sequential)]
    public struct INPUT { public uint type; public InputUnion U; }
    [StructLayout(LayoutKind.Explicit)]
    public struct InputUnion { [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public MOUSEINPUT mi; }
    [StructLayout(LayoutKind.Sequential)]
    public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [DllImport("user32.dll", SetLastError=true)] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
    public const uint KEYEVENTF_KEYUP = 0x0002;
    public const ushort VK_CONTROL = 0x11, VK_MENU = 0x12, VK_V = 0x56;
}
'@

function Get-PopupVisible([int]$procId) {
    $script:visible = $false
    $null = [WinFinal]::EnumWindows([WinFinal+EnumWindowsProc]{
      param($h, $l)
      $wpid = 0; [WinFinal]::GetWindowThreadProcessId($h, [ref]$wpid) | Out-Null
      if ($wpid -eq $procId) {
        $cn = New-Object System.Text.StringBuilder 256; [WinFinal]::GetClassName($h, $cn, 256) | Out-Null
        if ($cn.ToString() -eq "Window Class" -and [WinFinal]::IsWindowVisible($h)) { $script:visible = $true }
      }
      return $true
    }, [IntPtr]::Zero)
    return $script:visible
}

function Send-Hotkey {
    (New-Object -ComObject WScript.Shell).SendKeys("^%v")
}

Write-Host "=== [A] smoke: start / copy / latency / dedup / crash / restart ==="
$p = Start-Process -FilePath (Join-Path $PWD "target\release\clipx.exe") -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
if ($p.HasExited) { Write-Host "FAIL: exited at startup"; exit 1 }
$p.Refresh()
$wsMB=[math]::Round($p.WorkingSet64/1MB,1); $pmMB=[math]::Round($p.PrivateMemorySize64/1MB,1)
Write-Host "    idle memory: WS=${wsMB}MB private=${pmMB}MB"
if (Get-PopupVisible $p.Id) { Write-Host "FAIL: popup visible at startup"; Stop-Process -Id $p.Id -Force; exit 1 }
Write-Host "    popup hidden at startup: OK"

$stamp = "smoke-rel-final-{0}" -f (Get-Date -Format "HHmmss")
if (-not (Copy-Text $stamp)) { Write-Host "FAIL: copy failed"; Stop-Process -Id $p.Id -Force; exit 1 }
$t0 = Get-Date; $found = $false
while (((Get-Date) - $t0).TotalSeconds -lt 5) {
    if (sqlite3 $db "SELECT 1 FROM entries WHERE preview='$stamp' LIMIT 1;") { $found=$true; $ms=[math]::Round(((Get-Date)-$t0).TotalMilliseconds); break }
    Start-Sleep -Milliseconds 50
}
if (-not $found) { Write-Host "FAIL: not in DB"; Stop-Process -Id $p.Id -Force; exit 1 }
Write-Host "    insert latency: ${ms}ms"
Copy-Text $stamp | Out-Null
Start-Sleep -Seconds 2
$count = sqlite3 $db "SELECT COUNT(*) FROM entries WHERE preview='$stamp';"
Write-Host "    dedup rows: $count (expect 1)"
if ($count -ne "1") { Stop-Process -Id $p.Id -Force; exit 1 }

Write-Host "=== [B] hotkey toggle (Ctrl+Alt+V via SendInput) ==="
Send-Hotkey
Start-Sleep -Seconds 1
if (-not (Get-PopupVisible $p.Id)) { Write-Host "FAIL: popup not shown after hotkey"; Stop-Process -Id $p.Id -Force; exit 1 }
Write-Host "    hotkey shows popup: OK"
Send-Hotkey
Start-Sleep -Seconds 1
if (Get-PopupVisible $p.Id) { Write-Host "FAIL: popup not hidden after second hotkey"; Stop-Process -Id $p.Id -Force; exit 1 }
Write-Host "    hotkey hides popup: OK"
Start-Sleep -Seconds 2
if ($p.HasExited) { Write-Host "FAIL: process exited after hiding popup"; exit 1 }
Write-Host "    process stays alive (explicit shutdown mode): OK"

Write-Host "=== [C] S3: 100 consecutive copies ==="
$prefix = "s3-final-{0}" -f (Get-Date -Format "HHmmss")
$failCount = 0
for ($i = 1; $i -le 100; $i++) {
    if (-not (Copy-Text "$prefix-$i")) { $failCount++ }
}
Start-Sleep -Seconds 6
$inserted = sqlite3 $db "SELECT COUNT(*) FROM entries WHERE preview LIKE '$prefix-%';"
$distinct = sqlite3 $db "SELECT COUNT(DISTINCT preview) FROM entries WHERE preview LIKE '$prefix-%';"
Write-Host "    copy failures: $failCount, inserted: $inserted (expect 100), distinct: $distinct (expect 100)"
$p.Refresh()
$wsMB2=[math]::Round($p.WorkingSet64/1MB,1); $pmMB2=[math]::Round($p.PrivateMemorySize64/1MB,1)
Write-Host "    memory after burst: WS=${wsMB2}MB private=${pmMB2}MB"

Write-Host "=== [D] kill + WAL persistence + restart ==="
Stop-Process -Id $p.Id -Force
Start-Sleep -Seconds 1
$afterKill = sqlite3 $db "SELECT COUNT(*) FROM entries WHERE preview LIKE '$prefix-%';"
Write-Host "    rows after crash: $afterKill (expect 100)"
$p2 = Start-Process -FilePath (Join-Path $PWD "target\release\clipx.exe") -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 3
if ($p2.HasExited) { Write-Host "FAIL: restart failed"; exit 1 }
$total = sqlite3 $db "SELECT COUNT(*) FROM entries;"
$p2.Refresh()
$wsMB3=[math]::Round($p2.WorkingSet64/1MB,1); $pmMB3=[math]::Round($p2.PrivateMemorySize64/1MB,1)
Write-Host "    restart OK, total rows: $total, WS=${wsMB3}MB private=${pmMB3}MB"
Stop-Process -Id $p2.Id -Force

$pass = $true
if ($failCount -gt 5 -or $inserted -ne "100" -or $distinct -ne "100") { $pass=$false; Write-Host "FAIL: S3" }
if ($afterKill -ne "100") { $pass=$false; Write-Host "FAIL: WAL persistence" }
if ($wsMB2 -gt 30 -or $wsMB3 -gt 30) { $pass=$false; Write-Host "FAIL: memory budget" }

if ($pass) { Write-Host ""; Write-Host "=== ALL M0 ACCEPTANCE PASSED ===" } else { exit 1 }
