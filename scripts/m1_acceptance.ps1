$ErrorActionPreference = "Stop"
$exe = "target\release\clipx.exe"
$db  = "target\release\Data\clipx.db"

Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class M1Win {
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
    // C# 侧组装结构体：PowerShell 对嵌套值类型字段的赋值不会写回
    public static uint SendKey(ushort vk, bool up) {
        INPUT[] arr = new INPUT[1];
        arr[0].type = 1;
        arr[0].U.ki.wVk = vk;
        if (up) arr[0].U.ki.dwFlags = KEYEVENTF_KEYUP;
        return SendInput(1, arr, Marshal.SizeOf(typeof(INPUT)));
    }
    // 依序按下、逆序抬起；避免 WScript SendKeys 遗留修饰键按下状态
    public static uint SendCombo(ushort[] vks) {
        int n = vks.Length;
        INPUT[] arr = new INPUT[n * 2];
        for (int i = 0; i < n; i++) { arr[i].type = 1; arr[i].U.ki.wVk = vks[i]; }
        for (int i = 0; i < n; i++) {
            arr[n + i].type = 1;
            arr[n + i].U.ki.wVk = vks[n - 1 - i];
            arr[n + i].U.ki.dwFlags = KEYEVENTF_KEYUP;
        }
        return SendInput((uint)arr.Length, arr, Marshal.SizeOf(typeof(INPUT)));
    }
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);
    public const uint MOUSEEVENTF_LEFTDOWN = 0x0002, MOUSEEVENTF_LEFTUP = 0x0004;
}
'@

function Get-PopupVisible([int]$procId) {
    $script:found = 0
    $null = [M1Win]::EnumWindows([M1Win+EnumWindowsProc]{
      param($h, $l)
      $wpid = 0; [M1Win]::GetWindowThreadProcessId($h, [ref]$wpid) | Out-Null
      if ($wpid -eq $procId -and [M1Win]::IsWindowVisible($h)) {
        $cn = New-Object System.Text.StringBuilder 256; [M1Win]::GetClassName($h, $cn, 256) | Out-Null
        if ($cn.ToString() -eq "Window Class") { $script:found++ }
      }
      return $true
    }, [IntPtr]::Zero)
    return $script:found -gt 0
}

function Send-Hotkey {
    $n = [M1Win]::SendCombo([uint16[]]@(0x11, 0x12, 0x56))
    if ($n -ne 6) { throw "SendHotkey injected $n/6" }
}

function Send-Key([int]$vk) {
    $n = [M1Win]::SendKey([uint16]$vk, $false)
    $n2 = [M1Win]::SendKey([uint16]$vk, $true)
    if ($n -ne 1 -or $n2 -ne 1) { throw "SendKey vk=$($vk.ToString('X')) failed (down=$n up=$n2)" }
}

function Type-Text([string]$text) {
    # 字母的虚拟键码是大写 ASCII（'A'=0x41）；小写 0x61 是小键盘 1
    foreach ($c in $text.ToCharArray()) {
        Send-Key ([int][char]([char]::ToUpper($c)))
        Start-Sleep -Milliseconds 40
    }
}

function Copy-Text([string]$text) {
    for ($i = 0; $i -lt 10; $i++) {
        try { Set-Clipboard -Value $text; return $true } catch { Start-Sleep -Milliseconds 200 }
    }
    return $false
}

function Fail([string]$msg) {
    Write-Host "    FAIL: $msg" -ForegroundColor Red
    $script:pass = $false
}

$script:pass = $true

# ---------- [A] 清理旧实例并启动 ----------
Get-Process clipx -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
$p = Start-Process -FilePath (Join-Path $PWD $exe) -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
if ($p.HasExited) { Write-Host "FAIL: exited at startup"; exit 1 }
$p.Refresh()
$wsMB = [math]::Round($p.WorkingSet64/1MB,1)
Write-Host "=== [A] startup: WS=${wsMB}MB"
if ($wsMB -gt 30) { Fail "idle WS ${wsMB}MB > 30MB" }
if (Get-PopupVisible $p.Id) { Fail "popup visible at startup" }

# ---------- [B] 单实例 ----------
$p2 = Start-Process -FilePath (Join-Path $PWD $exe) -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
Write-Host "=== [B] single instance: second exited=$($p2.HasExited), first alive=$(-not $p.HasExited)"
if (-not $p2.HasExited) { Fail "second instance did not exit"; Stop-Process -Id $p2.Id -Force }
if ($p.HasExited) { Fail "first instance died" }

# ---------- [C] 复制三类文本 ----------
Copy-Text "你好世界" | Out-Null;   Start-Sleep -Milliseconds 300
Copy-Text "部署 dev 环境" | Out-Null; Start-Sleep -Milliseconds 300
Copy-Text "hello m1 smoke" | Out-Null; Start-Sleep -Milliseconds 800
$t0 = Get-Date; $found = $false
while (((Get-Date) - $t0).TotalSeconds -lt 5) {
    $n = sqlite3 $db "SELECT COUNT(*) FROM entries WHERE preview='hello m1 smoke';"
    if ($n -eq "1") { $found = $true; break }
    Start-Sleep -Milliseconds 100
}
Write-Host "=== [C] copy capture: hello-in-db=$found, 你好=$(sqlite3 $db "SELECT COUNT(*) FROM entries WHERE preview='你好世界';"), 部署=$(sqlite3 $db "SELECT COUNT(*) FROM entries WHERE preview='部署 dev 环境';")"
if (-not $found) { Fail "copied text not captured" }

# ---------- [D] 热键弹出且不抢焦点 ----------
$fgBefore = [M1Win]::GetForegroundWindow()
Send-Hotkey; Start-Sleep -Milliseconds 900
$visible = Get-PopupVisible $p.Id
$fgAfter = [M1Win]::GetForegroundWindow()
Write-Host "=== [D] hotkey: visible=$visible, focus-kept=$($fgBefore -eq $fgAfter)"
if (-not $visible) { Fail "popup not shown after hotkey" }
if ($fgBefore -ne $fgAfter) { Fail "popup stole focus" }

# ---------- [E] 键盘钩子输入拼音搜索 + Enter 粘贴 ----------
Type-Text "shijie"; Start-Sleep -Milliseconds 300
Send-Key 0x0D; Start-Sleep -Milliseconds 900
$clip = Get-Clipboard -Raw
$visibleAfter = Get-PopupVisible $p.Id
Write-Host "=== [E] pinyin search + Enter paste: clipboard='$clip', popup-hidden=$(-not $visibleAfter)"
if ($clip -ne "你好世界") { Fail "pinyin search paste got '$clip' instead of 你好世界" }
if ($visibleAfter) { Fail "popup not hidden after Enter" }

# ---------- [F] 拼音首字母搜索 ----------
Send-Hotkey; Start-Sleep -Milliseconds 900
Type-Text "nh"; Start-Sleep -Milliseconds 300
Send-Key 0x0D; Start-Sleep -Milliseconds 900
$clip = Get-Clipboard -Raw
Write-Host "=== [F] initials search 'nh': clipboard='$clip'"
if ($clip -ne "你好世界") { Fail "initials search paste got '$clip'" }

# ---------- [G] 数字快贴（无搜索时按 1） ----------
Send-Hotkey; Start-Sleep -Milliseconds 900
Send-Key ([int][char]'1'); Start-Sleep -Milliseconds 900
$clip = Get-Clipboard -Raw
Write-Host "=== [G] digit quick paste: clipboard='$clip'"
if ($clip -ne "hello m1 smoke") { Fail "quick paste got '$clip' instead of most recent" }

# ---------- [H] Esc 关闭 ----------
Send-Hotkey; Start-Sleep -Milliseconds 900
if (-not (Get-PopupVisible $p.Id)) { Fail "popup not shown before Esc" }
Send-Key 0x1B; Start-Sleep -Milliseconds 600
$hidden = -not (Get-PopupVisible $p.Id)
Write-Host "=== [H] Esc close: hidden=$hidden"
if (-not $hidden) { Fail "Esc did not close popup" }

# ---------- [I] Delete 删除选中项 ----------
$topBefore = sqlite3 $db "SELECT preview FROM entries ORDER BY pinned DESC, created_ms DESC LIMIT 1;"
Send-Hotkey; Start-Sleep -Milliseconds 900
Send-Key 0x2E; Start-Sleep -Milliseconds 700
$still = sqlite3 $db "SELECT COUNT(*) FROM entries WHERE preview='$topBefore';"
Send-Key 0x1B; Start-Sleep -Milliseconds 300
Write-Host "=== [I] Delete key: removed='$topBefore', still-in-db=$still"
if ($still -ne "0") { Fail "Delete did not remove top entry" }

# ---------- [J] 点击外部关闭 ----------
Send-Hotkey; Start-Sleep -Milliseconds 900
if (-not (Get-PopupVisible $p.Id)) { Fail "popup not shown before outside click" }
[M1Win]::SetCursorPos(60, 60) | Out-Null
[M1Win]::mouse_event([M1Win]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
[M1Win]::mouse_event([M1Win]::MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 800
$hidden = -not (Get-PopupVisible $p.Id)
Write-Host "=== [J] outside click close: hidden=$hidden"
if (-not $hidden) { Fail "outside click did not close popup" }

# ---------- [K] 收尾 ----------
if ($p.HasExited) { Fail "process exited during tests" }
$p.Refresh()
$wsMB2 = [math]::Round($p.WorkingSet64/1MB,1)
Write-Host "=== [K] final: alive=$(-not $p.HasExited), WS=${wsMB2}MB"
if ($wsMB2 -gt 35) { Fail "final WS ${wsMB2}MB > 35MB" }
Stop-Process -Id $p.Id -Force

if ($script:pass) { Write-Host ""; Write-Host "=== ALL M1 ACCEPTANCE PASSED ===" } else { exit 1 }
