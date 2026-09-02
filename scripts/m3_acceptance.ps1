$ErrorActionPreference = "Stop"
$exe = "target\release\clipx.exe"
$db  = "target\release\Data\clipx.db"
$wpfDb = Join-Path $env:LOCALAPPDATA "ClipboardX\clipboard_history.db"

# M3 验收：万条压测 / WPF 迁移幂等 / 文件列表采集 / 富文本采集+回写 /
#           Ctrl+P 置顶 / Menu 键菜单 / 常驻内存 / UI 截图
# 需 STA：powershell -STA -File scripts\m3_acceptance.ps1
# UI 段（F/G/H/J）依赖 SendInput/BitBlt，须在用户自己的交互终端运行；
# TRAE 工具宿主等受限环境只能跑剪贴板段（PARTIAL）。

Add-Type -AssemblyName System.Windows.Forms

if ($Host.Runspace.ApartmentState -ne 'STA') {
    Write-Host "WARN: 当前非 STA（$($Host.Runspace.ApartmentState)），剪贴板写入可能失败" -ForegroundColor Yellow
}

Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class M3Win {
    public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder sb, int max);
    [StructLayout(LayoutKind.Sequential)]
    public struct INPUT { public uint type; public InputUnion U; }
    [StructLayout(LayoutKind.Explicit)]
    public struct InputUnion { [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public MOUSEINPUT mi; }
    [StructLayout(LayoutKind.Sequential)]
    public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    public struct MOUSEINPUT { public int dx; public int dy; public int mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [DllImport("user32.dll", SetLastError=true)] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
    public const uint KEYEVENTF_KEYUP = 0x0002;
    public static uint SendKey(ushort vk, bool up) {
        INPUT[] arr = new INPUT[1];
        arr[0].type = 1;
        arr[0].U.ki.wVk = vk;
        if (up) arr[0].U.ki.dwFlags = KEYEVENTF_KEYUP;
        return SendInput(1, arr, Marshal.SizeOf(typeof(INPUT)));
    }
    public static uint SendCombo(ushort[] vks) {
        int n = vks.Length;
        INPUT[] arr = new INPUT[n * 2];
        for (int i = 0; i < n; i++) { arr[i].type = 1; arr[i].U.ki.wVk = vks[i]; }
        for (int i = 0; i < n; i++) {
            arr[n + i].type = 1; arr[n + i].U.ki.wVk = vks[n - 1 - i];
            arr[n + i].U.ki.dwFlags = KEYEVENTF_KEYUP;
        }
        return SendInput((uint)arr.Length, arr, Marshal.SizeOf(typeof(INPUT)));
    }
    // --- 原始剪贴板直写（模拟 Chrome/Word 的真实写法）---
    // WinForms SetDataObject 走 OLE 延迟渲染：私有命名格式不落原始剪贴板，
    // 且渲染回调与监听方（clipx/rdpclip）竞争会耗尽 SetDataObject 内部重试。
    [DllImport("user32.dll", SetLastError=true)] public static extern bool OpenClipboard(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool CloseClipboard();
    [DllImport("user32.dll")] public static extern bool EmptyClipboard();
    // CharSet 必须显式 Unicode：默认 ANSI 编组会把 "HTML Format" 变成乱码名格式
    // （实测注册成 50149 而真 HTML Format 是 49416），导致脚本写的根本不是富文本
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern uint RegisterClipboardFormatW(string fmt);
    [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr SetClipboardData(uint fmt, IntPtr data);
    [DllImport("kernel32.dll")] public static extern IntPtr GlobalAlloc(uint flags, UIntPtr bytes);
    [DllImport("kernel32.dll")] public static extern IntPtr GlobalLock(IntPtr h);
    [DllImport("kernel32.dll")] public static extern bool GlobalUnlock(IntPtr h);
    public static bool OpenWithRetry(int attempts, int delayMs) {
        for (int i = 0; i < attempts; i++) {
            if (OpenClipboard(IntPtr.Zero)) return true;
            System.Threading.Thread.Sleep(delayMs);
        }
        return false;
    }
    static IntPtr ToHGlobal(byte[] bytes) {
        IntPtr h = GlobalAlloc(0x0002, (UIntPtr)bytes.Length);
        Marshal.Copy(bytes, 0, GlobalLock(h), bytes.Length);
        GlobalUnlock(h);
        return h;
    }
    /// CF_HTML 头部按固定宽度十进制偏移（两遍法：先算头长再填偏移）
    public static string BuildCfHtml(string fragment) {
        string header = "Version:0.9\r\nStartHTML:{0:d10}\r\nEndHTML:{1:d10}\r\nStartFragment:{2:d10}\r\nEndFragment:{3:d10}\r\n";
        string head = string.Format(header, 0, 0, 0, 0);
        int headLen = Encoding.UTF8.GetByteCount(head);
        string pre = "<html><body>\r\n<!--StartFragment-->";
        string post = "<!--EndFragment-->\r\n</body></html>";
        int startHtml = headLen;
        int startFrag = startHtml + Encoding.UTF8.GetByteCount(pre);
        int endFrag = startFrag + Encoding.UTF8.GetByteCount(fragment);
        int endHtml = endFrag + Encoding.UTF8.GetByteCount(post);
        return string.Format(header, startHtml, endHtml, startFrag, endFrag) + pre + fragment + post;
    }
    public static bool SetTextAndHtml(string text, string htmlFragment) {
        if (!OpenWithRetry(20, 100)) return false;
        try {
            EmptyClipboard();
            byte[] textBytes = Encoding.Unicode.GetBytes(text + "\0");
            if (SetClipboardData(13, ToHGlobal(textBytes)) == IntPtr.Zero) return false;
            uint htmlFmt = RegisterClipboardFormatW("HTML Format");
            byte[] htmlBytes = Encoding.UTF8.GetBytes(BuildCfHtml(htmlFragment));
            if (SetClipboardData(htmlFmt, ToHGlobal(htmlBytes)) == IntPtr.Zero) return false;
            return true;
        } finally { CloseClipboard(); }
    }
}
'@

function Get-PopupVisible([int]$procId) {
    $script:found = 0
    $null = [M3Win]::EnumWindows([M3Win+EnumWindowsProc]{
      param($h, $l)
      $wpid = 0; [M3Win]::GetWindowThreadProcessId($h, [ref]$wpid) | Out-Null
      if ($wpid -eq $procId -and [M3Win]::IsWindowVisible($h)) {
        $cn = New-Object System.Text.StringBuilder 256; [M3Win]::GetClassName($h, $cn, 256) | Out-Null
        if ($cn.ToString() -eq "Window Class") { $script:found++ }
      }
      return $true
    }, [IntPtr]::Zero)
    return $script:found -gt 0
}

function Send-Hotkey {
    $n = [M3Win]::SendCombo([uint16[]]@(0x11, 0x12, 0x56))
    if ($n -ne 6) { throw "SendHotkey injected $n/6" }
}

function Send-Key([int]$vk) {
    $n = [M3Win]::SendKey([uint16]$vk, $false)
    $n2 = [M3Win]::SendKey([uint16]$vk, $true)
    if ($n -ne 1 -or $n2 -ne 1) { throw "SendKey vk=$($vk.ToString('X')) failed" }
}

function Type-Text([string]$text) {
    foreach ($c in $text.ToCharArray()) {
        Send-Key ([int][char]([char]::ToUpper($c)))
        Start-Sleep -Milliseconds 40
    }
}

function Fail([string]$msg) {
    Write-Host "    FAIL: $msg" -ForegroundColor Red
    $script:pass = $false
}

function Wait-Db([string]$sql, [string]$expect, [float]$timeoutSec = 8) {
    $t0 = Get-Date
    while (((Get-Date) - $t0).TotalSeconds -lt $timeoutSec) {
        $v = sqlite3 $db $sql
        if ($v -eq $expect) { return $true }
        Start-Sleep -Milliseconds 150
    }
    return $false
}

function Run-ClipxCapture([string[]]$cliArgs, [ref]$stdout) {
    $tmp = [System.IO.Path]::GetTempFileName()
    $p = Start-Process -FilePath (Join-Path $PWD $exe) -ArgumentList $cliArgs -Wait -PassThru -NoNewWindow -RedirectStandardOutput $tmp
    $stdout.Value = (Get-Content $tmp -Raw -Encoding UTF8)
    Remove-Item $tmp -Force
    return $p.ExitCode
}

$script:pass = $true

# ---------- [0] 输入注入预检：SendInput 被拒时 UI 段（F/G/H/J）必失败 ----------
# 两种成因：a) 机器锁屏/安全桌面；b) 受限执行环境（如 TRAE 工具宿主进程的
# 窗口站权限被裁剪：SendInput/GetCursorPos/BitBlt 全部 err=5，EnumWindows 却正常）。
# 剪贴板段照常可测；UI 交互段必须换到用户自己的交互终端里跑本脚本。
$script:uiBlocked = $false
$shiftProbe = [M3Win]::SendCombo([uint16[]]@(0x10))  # Shift 按下+抬起（无副作用探针）
if ($shiftProbe -ne 2) {
    $script:uiBlocked = $true
    Write-Host "=== [0] 输入注入被拒（SendInput=$shiftProbe/2）：锁屏或受限执行环境，F/G/H/J 段将跳过" -ForegroundColor Yellow
} else {
    Write-Host "=== [0] 输入通道正常"
}

# ---------- [A] 万条压测（搜索 <100ms 验收线）----------
Write-Host "=== [A] bench: 10000 条压测"
$out = [ref]""
$null = Run-ClipxCapture @("--bench") $out
Write-Host ($out.Value.Trim() -replace "`n", " | ")
if ($out.Value -match "最慢路径\s+([0-9.]+)ms") {
    $ms = [double]$Matches[1]
    if ($ms -ge 100) { Fail "bench slowest ${ms}ms >= 100ms" }
} else {
    Fail "bench output unparsable"
}

# ---------- [B] WPF 迁移 + 幂等 ----------
Get-Process clipx -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Get-ChildItem "$db*" -ErrorAction SilentlyContinue | Remove-Item -Force

if (Test-Path $wpfDb) {
    Write-Host "=== [B] import-wpf（真实 WPF 库 $wpfDb）"
    $out = [ref]""
    $null = Run-ClipxCapture @("--import-wpf", $wpfDb) $out
    Write-Host $out.Value.Trim()
    if ($out.Value -notmatch "新增 (\d+) 条") { Fail "import run1 output unparsable" }
    $first = [int]$Matches[1]
    # 幂等：第二次运行必须零新增（content_hash 去重）
    $null = Run-ClipxCapture @("--import-wpf", $wpfDb) $out
    Write-Host "second run: $($out.Value.Trim())"
    if ($out.Value -notmatch "新增 0 条") { Fail "import not idempotent" }
    # 迁移无丢失：分 kind 比对唯一内容数
    # （WPF 库自身存在同内容多条——批内 hash 去重后只保留一条，属预期）
    $wpfText = [int](sqlite3 $wpfDb "SELECT COUNT(DISTINCT COALESCE(text_content,'')) FROM clipboard_history WHERE entry_type=0;")
    $wpfFiles = [int](sqlite3 $wpfDb "SELECT COUNT(DISTINCT COALESCE(file_paths_json,'')) FROM clipboard_history WHERE entry_type=2;")
    $wpfImg = [int](sqlite3 $wpfDb "SELECT COUNT(DISTINCT LENGTH(image_blob)) FROM clipboard_history WHERE entry_type=1;")
    $cxText = [int](sqlite3 $db "SELECT COUNT(DISTINCT COALESCE(full_text,'')) FROM payloads p JOIN entries e ON e.id=p.entry_id WHERE e.kind=0;")
    $cxFiles = [int](sqlite3 $db "SELECT COUNT(DISTINCT COALESCE(full_text,'')) FROM payloads p JOIN entries e ON e.id=p.entry_id WHERE e.kind=2;")
    $cxImg = [int](sqlite3 $db "SELECT COUNT(*) FROM entries WHERE kind=1;")
    Write-Host "unique text: wpf=$wpfText clipx=$cxText / files: wpf=$wpfFiles clipx=$cxFiles / images: wpf=$wpfImg clipx=$cxImg"
    if ($cxText -lt $wpfText) { Fail "text rows lost ($cxText < $wpfText)" }
    if ($cxFiles -lt $wpfFiles) { Fail "file rows lost ($cxFiles < $wpfFiles)" }
    if ($cxImg -lt $wpfImg) { Fail "image rows lost ($cxImg < $wpfImg)" }
    # 容量设置已同步（否则首次新插入会按默认 2000 裁掉历史）
    $cfg = Get-Content "target\release\Data\settings.json" -Raw -Encoding UTF8 | ConvertFrom-Json
    Write-Host "clipx max_items=$($cfg.max_items)"
    if ($cfg.max_items -lt 6693) { Fail "max_items not synced from WPF (got $($cfg.max_items))" }
} else {
    Write-Host "=== [B] 跳过（未找到 WPF 库）" -ForegroundColor Yellow
}

# ---------- [C] 启动 + 常驻内存 ----------
$p = Start-Process -FilePath (Join-Path $PWD $exe) -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
if ($p.HasExited) { Write-Host "FAIL: exited at startup"; exit 1 }
# 迁移图片的 OCR backfill 突发：等队列排空（每完成一条 ocr_state 置 1/2）
$ocrBacklog = 0
$t0 = Get-Date
while (((Get-Date) - $t0).TotalSeconds -lt 90) {
    $ocrBacklog = [int](sqlite3 $db "SELECT COUNT(*) FROM entries WHERE kind=1 AND ocr_state=0;")
    if ($ocrBacklog -eq 0) { break }
    Start-Sleep -Milliseconds 500
}
Write-Host "=== [C] ocr backlog=${ocrBacklog}（等待 $([int]((Get-Date)-$t0).TotalSeconds)s）"
# 隐藏后空闲 trim（hidden_at + 5s）触发一次工作集归还，等它落地
Start-Sleep -Seconds 7
$p.Refresh(); $wsMB = [math]::Round($p.WorkingSet64/1MB,1)
Write-Host "=== [C] startup settled: WS=${wsMB}MB"
if ($wsMB -gt 35) { Fail "idle WS ${wsMB}MB > 35MB" }

# ---------- [D] 文件列表采集（CF_HDROP）----------
Write-Host "=== [D] 文件列表采集"
$f1 = Join-Path $env:TEMP "m3_file_a.txt"
$f2 = Join-Path $env:TEMP "m3_file_b.txt"
"hello m3 a" | Set-Content $f1 -Encoding UTF8
"hello m3 b" | Set-Content $f2 -Encoding UTF8
$files = New-Object System.Collections.Specialized.StringCollection
[void]$files.Add($f1); [void]$files.Add($f2)
$dropOk = $false
for ($i = 0; $i -lt 10; $i++) {
    try { [System.Windows.Forms.Clipboard]::SetFileDropList($files); $dropOk = $true; break }
    catch { Start-Sleep -Milliseconds 300 }
}
if (-not $dropOk) { Fail "SetFileDropList failed (clipboard busy)" }
$inDb = Wait-Db "SELECT COUNT(*) FROM entries e JOIN payloads p ON p.entry_id=e.id WHERE e.kind=2 AND p.full_text LIKE '%m3_file_a.txt%';" "1"
if (-not $inDb) { Fail "CF_HDROP file list not captured (kind=2)" }

# ---------- [E] 富文本采集（原始直写 CF_UNICODETEXT + HTML Format，同 Chrome/Word）----------
Write-Host "=== [E] 富文本采集"
$richOk = [M3Win]::SetTextAndHtml("M3RichText 主体内容", "<b>M3RichText</b> 主体内容")
if (-not $richOk) { Fail "raw SetTextAndHtml failed (clipboard busy)" }
$inDb = Wait-Db "SELECT COUNT(*) FROM entries e JOIN payloads p ON p.entry_id=e.id WHERE e.kind=3 AND p.full_text LIKE '%M3RichText%';" "1"
if (-not $inDb) { Fail "rich text (kind=3) not captured" }
$htmlOk = (sqlite3 $db "SELECT COUNT(*) FROM payloads WHERE html LIKE '%M3RichText%';") -eq "1"
if (-not $htmlOk) { Fail "rich text html payload not stored" }

# ---------- [F] 富文本粘贴回写（搜索 + Enter → HTML Format 回写）----------
if ($script:uiBlocked) {
    Write-Host "=== [F] 跳过（锁屏）" -ForegroundColor Yellow
} else {
Write-Host "=== [F] 富文本回写"
Send-Hotkey; Start-Sleep -Milliseconds 600
if (-not (Get-PopupVisible $p.Id)) { Fail "popup not visible after hotkey" }
Type-Text "M3Rich"; Start-Sleep -Milliseconds 500
Send-Key 0x0D; Start-Sleep -Milliseconds 800
if (Get-PopupVisible $p.Id) { Fail "popup not hidden after Enter" }
$hasHtml = [System.Windows.Forms.Clipboard]::ContainsText([System.Windows.Forms.TextDataFormat]::Html)
$clipText = [System.Windows.Forms.Clipboard]::GetText([System.Windows.Forms.TextDataFormat]::UnicodeText)
if (-not $hasHtml) { Fail "paste-back did not write HTML Format" }
if ($clipText -notlike "*M3RichText 主体内容*") { Fail "paste-back text projection wrong: '$clipText'" }
# 回写被 Gate 吸收，不产生新条目
Start-Sleep -Milliseconds 800
$cnt = sqlite3 $db "SELECT COUNT(*) FROM entries e JOIN payloads p ON p.entry_id=e.id WHERE e.kind=3 AND p.full_text LIKE '%M3RichText%';"
if ($cnt -ne "1") { Fail "gate did not absorb self-write (kind=3 rows: $cnt)" }
}

# ---------- [G] Ctrl+P 置顶（选中行 = 最新条目）----------
if ($script:uiBlocked) {
    Write-Host "=== [G] 跳过（锁屏）" -ForegroundColor Yellow
} else {
Write-Host "=== [G] Ctrl+P 置顶"
Send-Hotkey; Start-Sleep -Milliseconds 600
if (-not (Get-PopupVisible $p.Id)) { Fail "popup not visible (G)" }
$pinSql = "SELECT pinned FROM entries WHERE kind=3 AND preview LIKE '%M3RichText%' ORDER BY created_ms DESC LIMIT 1;"
# 确保未置顶起点
sqlite3 $db "UPDATE entries SET pinned=0 WHERE kind=3 AND preview LIKE '%M3RichText%';" | Out-Null
# 弹窗快照在 show_popup 时已取，Ctrl+P 作用于选中行 0（最新）→ 该条目
[M3Win]::SendCombo([uint16[]]@(0x11, 0x50)) | Out-Null
Start-Sleep -Milliseconds 500
if ((sqlite3 $db $pinSql) -ne "1") { Fail "Ctrl+P did not pin newest entry" }
[M3Win]::SendCombo([uint16[]]@(0x11, 0x50)) | Out-Null
Start-Sleep -Milliseconds 500
if ((sqlite3 $db $pinSql) -ne "0") { Fail "second Ctrl+P did not unpin" }
Send-Key 0x1B; Start-Sleep -Milliseconds 400
}

# ---------- [H] Menu 键菜单：Esc 只关菜单不关弹窗 ----------
if ($script:uiBlocked) {
    Write-Host "=== [H] 跳过（锁屏）" -ForegroundColor Yellow
} else {
Write-Host "=== [H] Menu 键菜单"
Send-Hotkey; Start-Sleep -Milliseconds 600
if (-not (Get-PopupVisible $p.Id)) { Fail "popup not visible (H)" }
Send-Key 0x5D; Start-Sleep -Milliseconds 400     # Menu 键 → 打开菜单
Send-Key 0x1B; Start-Sleep -Milliseconds 400     # Esc → 只关菜单
if (-not (Get-PopupVisible $p.Id)) { Fail "Esc closed popup while menu open (should close menu only)" }
if ($p.HasExited) { Fail "process died during menu interaction" }
Send-Key 0x1B; Start-Sleep -Milliseconds 400     # 再 Esc → 关弹窗
if (Get-PopupVisible $p.Id) { Fail "popup not hidden after second Esc" }
}

# ---------- [I] 收尾内存 ----------
# 等隐藏后 5s 周期的空闲 trim 落地再取值
Start-Sleep -Seconds 6
$p.Refresh(); $wsEnd = [math]::Round($p.WorkingSet64/1MB,1)
Write-Host "=== [I] final WS=${wsEnd}MB"
if ($wsEnd -gt 35) { Fail "final WS ${wsEnd}MB > 35MB" }

# ---------- [J] UI 截图（真实热键路径弹窗 → 截屏 → Esc 收起，供视觉验收）----------
if ($script:uiBlocked) {
    Write-Host "=== [J] 跳过（输入/截屏受限）" -ForegroundColor Yellow
} else {
Write-Host "=== [J] UI 截图"
Send-Hotkey; Start-Sleep -Milliseconds 900
if (Get-PopupVisible $p.Id) {
    Add-Type -AssemblyName System.Drawing
    Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public class Cap { [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p); [DllImport("user32.dll")] public static extern IntPtr MonitorFromPoint(POINT p, uint flags); [DllImport("user32.dll")] public static extern bool GetMonitorInfoW(IntPtr h, ref MONINFO mi); [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; } [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; } [StructLayout(LayoutKind.Sequential)] public struct MONINFO { public int cbSize; public RECT rcMonitor; public RECT rcWork; public uint dwFlags; } }'
    $pt = New-Object Cap+POINT
    [Cap]::GetCursorPos([ref]$pt) | Out-Null
    $mon = [Cap]::MonitorFromPoint($pt, 2)
    $mi = New-Object Cap+MONINFO
    $mi.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf($mi)
    [Cap]::GetMonitorInfoW($mon, [ref]$mi) | Out-Null
    $mw = $mi.rcMonitor.R - $mi.rcMonitor.L; $mh = $mi.rcMonitor.B - $mi.rcMonitor.T
    $bmp = New-Object System.Drawing.Bitmap $mw, $mh
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($mi.rcMonitor.L, $mi.rcMonitor.T, 0, 0, $bmp.Size)
    $g.Dispose()
    $shot = Join-Path $PWD "target\uitest_popup.png"
    $bmp.Save($shot, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "    截图已存 $shot"
    Send-Key 0x1B; Start-Sleep -Milliseconds 400
} else {
    Fail "popup not visible for screenshot (J)"
}
}

Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Remove-Item $f1, $f2 -Force -ErrorAction SilentlyContinue

if ($script:uiBlocked) {
    if ($script:pass) {
        Write-Host "M3 ACCEPTANCE: PARTIAL（剪贴板段全过；F/G/H/J UI 段因锁屏/受限环境跳过，请在用户自己的交互终端重跑取全量结果）" -ForegroundColor Yellow
        exit 2
    }
    Write-Host "M3 ACCEPTANCE: FAIL" -ForegroundColor Red; exit 1
}
if ($script:pass) { Write-Host "M3 ACCEPTANCE: PASS" -ForegroundColor Green }
else { Write-Host "M3 ACCEPTANCE: FAIL" -ForegroundColor Red; exit 1 }
