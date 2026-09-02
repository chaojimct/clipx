$ErrorActionPreference = "Stop"
$exe = "target\release\clipx.exe"
$db  = "target\release\Data\clipx.db"

# M2 验收：图片采集/缩略图/OCR 可搜/Space 预览/4K 不崩/100 图内存回落
# 需在 STA 会话运行（图片剪贴板写入依赖）：
#   powershell -STA -File scripts\m2_acceptance.ps1

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

if ($Host.Runspace.ApartmentState -ne 'STA') {
    Write-Host "WARN: 当前非 STA（$($Host.Runspace.ApartmentState)），图片写入剪贴板可能失败" -ForegroundColor Yellow
}

Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class M2Win {
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
            arr[n + i].type = 1;
            arr[n + i].U.ki.wVk = vks[n - 1 - i];
            arr[n + i].U.ki.dwFlags = KEYEVENTF_KEYUP;
        }
        return SendInput((uint)arr.Length, arr, Marshal.SizeOf(typeof(INPUT)));
    }
}
'@

function Get-PopupVisible([int]$procId) {
    $script:found = 0
    $null = [M2Win]::EnumWindows([M2Win+EnumWindowsProc]{
      param($h, $l)
      $wpid = 0; [M2Win]::GetWindowThreadProcessId($h, [ref]$wpid) | Out-Null
      if ($wpid -eq $procId -and [M2Win]::IsWindowVisible($h)) {
        $cn = New-Object System.Text.StringBuilder 256; [M2Win]::GetClassName($h, $cn, 256) | Out-Null
        if ($cn.ToString() -eq "Window Class") { $script:found++ }
      }
      return $true
    }, [IntPtr]::Zero)
    return $script:found -gt 0
}

function Send-Hotkey {
    $n = [M2Win]::SendCombo([uint16[]]@(0x11, 0x12, 0x56))
    if ($n -ne 6) { throw "SendHotkey injected $n/6" }
}

function Send-Key([int]$vk) {
    $n = [M2Win]::SendKey([uint16]$vk, $false)
    $n2 = [M2Win]::SendKey([uint16]$vk, $true)
    if ($n -ne 1 -or $n2 -ne 1) { throw "SendKey vk=$($vk.ToString('X')) failed" }
}

function Type-Text([string]$text) {
    foreach ($c in $text.ToCharArray()) {
        Send-Key ([int][char]([char]::ToUpper($c)))
        Start-Sleep -Milliseconds 40
    }
}

function New-TextImage([int]$w, [int]$h, [string]$text, [float]$pt = 40) {
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::White)
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::ClearTypeGridFit
    $font = New-Object System.Drawing.Font("Arial", $pt, [System.Drawing.FontStyle]::Bold)
    $g.DrawString($text, $font, [System.Drawing.Brushes]::Black, [float]10, [float]10)
    $g.Dispose(); $font.Dispose()
    return $bmp
}

function Set-ClipImage([System.Drawing.Bitmap]$bmp) {
    for ($i = 0; $i -lt 10; $i++) {
        try { [System.Windows.Forms.Clipboard]::SetImage($bmp); return $true }
        catch { Start-Sleep -Milliseconds 200 }
    }
    return $false
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

$script:pass = $true

# ---------- [A] 清理 + 启动 ----------
Get-Process clipx -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Get-ChildItem "$db*" -ErrorAction SilentlyContinue | Remove-Item -Force
$p = Start-Process -FilePath (Join-Path $PWD $exe) -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
if ($p.HasExited) { Write-Host "FAIL: exited at startup"; exit 1 }
$p.Refresh(); $wsMB = [math]::Round($p.WorkingSet64/1MB,1)
Write-Host "=== [A] startup: WS=${wsMB}MB"
if ($wsMB -gt 30) { Fail "idle WS ${wsMB}MB > 30MB" }

# ---------- [B] 截图（带文字图片）采集 + 缩略图 ----------
$bmp = New-TextImage 800 200 "ClipxM2 OCR Verify"
$ok = Set-ClipImage $bmp
$bmp.Dispose()
$inDb = Wait-Db "SELECT COUNT(*) FROM entries WHERE kind=1" "1" 8
$thumbOk = (sqlite3 $db "SELECT COUNT(*) FROM payloads WHERE thumb_blob IS NOT NULL AND length(thumb_blob) > 8;") -eq "1"
Write-Host "=== [B] image capture: clipboard-set=$ok, in-db=$inDb, thumb=$thumbOk"
if (-not $inDb) { Fail "captured image not stored" }
if (-not $thumbOk) { Fail "thumbnail not generated" }

# ---------- [C] OCR 完成 + 文本可搜 ----------
# 注：匹配词用 "OCR Verify"——实测 Arial 40pt 下 Media OCR 会把 "ClipxM2" 的 l 读成 I，
# 用无歧义子串断言 OCR 文本链路本身。
$ocrSql = "SELECT COUNT(*) FROM payloads p JOIN entries e ON e.id=p.entry_id WHERE p.ocr_text LIKE '%OCR Verify%' AND e.ocr_state=2;"
$t0 = Get-Date
$ocrDone = $false
while (((Get-Date) - $t0).TotalSeconds -lt 20) {
    if ((sqlite3 $db $ocrSql) -eq "1") { $ocrDone = $true; break }
    Start-Sleep -Milliseconds 300
}
$ocrSec = [math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
$ocrText = sqlite3 $db "SELECT ocr_text FROM payloads WHERE ocr_text IS NOT NULL LIMIT 1;"
Write-Host "=== [C] OCR: done=$ocrDone in ${ocrSec}s, text='$ocrText'"
if (-not $ocrDone) { Fail "OCR did not complete in 20s" }

# ---------- [D] OCR 词搜索 + Enter 粘贴图片回写 ----------
# 先写文本哨兵：Enter 后剪贴板必须是图片，才能证明是回写而非 [B] 残留
[System.Windows.Forms.Clipboard]::SetText("M2-SENTINEL") | Out-Null
Start-Sleep -Milliseconds 400
Send-Hotkey; Start-Sleep -Milliseconds 900
Type-Text "verify"; Start-Sleep -Milliseconds 400
Send-Key 0x0D; Start-Sleep -Milliseconds 900
$img = $null
try { $img = Get-Clipboard -Format Image } catch {}
$visibleAfter = Get-PopupVisible $p.Id
$dim = if ($img) { "$($img.Width)x$($img.Height)" } else { "none" }
Write-Host "=== [D] ocr-search paste: clipboard-image=$dim, popup-hidden=$(-not $visibleAfter)"
if (-not $img) { Fail "search by OCR text did not paste an image back" }
elseif ($img.Width -ne 800) { Fail "pasted image width $($img.Width) != 800" }
if ($visibleAfter) { Fail "popup not hidden after Enter" }
if ($img) { $img.Dispose() }

# ---------- [E] Space 预览开关 ----------
Send-Hotkey; Start-Sleep -Milliseconds 900
Send-Key 0x20; Start-Sleep -Milliseconds 600   # Space 开预览
$previewShown = -not $p.HasExited -and (Get-PopupVisible $p.Id)
Send-Key 0x20; Start-Sleep -Milliseconds 300   # Space 关预览
Send-Key 0x1B; Start-Sleep -Milliseconds 500   # Esc 关弹窗
Write-Host "=== [E] Space preview toggle: alive+visible=$previewShown"
if (-not $previewShown) { Fail "Space preview crashed or closed popup" }

# ---------- [F] 4K 大图预览 ----------
$big = New-TextImage 3840 2160 "4K Preview Test" 120
Set-ClipImage $big | Out-Null
$big.Dispose()
$inDb4k = Wait-Db "SELECT COUNT(*) FROM entries WHERE kind=1" "2" 8
Send-Hotkey; Start-Sleep -Milliseconds 900
$t0 = Get-Date
Send-Key 0x20; Start-Sleep -Milliseconds 1200   # 预览 4K 图（含解码）
$previewMs = [math]::Round(((Get-Date) - $t0).TotalMilliseconds)
$p.Refresh(); $ws4k = [math]::Round($p.WorkingSet64/1MB,1)
$alive4k = -not $p.HasExited -and (Get-PopupVisible $p.Id)
Send-Key 0x1B; Start-Sleep -Milliseconds 400
Write-Host "=== [F] 4K preview: in-db=$inDb4k, alive=$alive4k, WS=${ws4k}MB"
if (-not $inDb4k) { Fail "4K image not captured" }
if (-not $alive4k) { Fail "process died or popup closed during 4K preview" }

# ---------- [G] 100 张连发 → 内存回落 ----------
$sw = [System.Diagnostics.Stopwatch]::StartNew()
for ($i = 0; $i -lt 100; $i++) {
    $b = New-TextImage 320 200 ("Burst " + $i.ToString("000"))
    Set-ClipImage $b | Out-Null
    $b.Dispose()
    Start-Sleep -Milliseconds 120
}
$sw.Stop()
$burstSec = [math]::Round($sw.Elapsed.TotalSeconds, 1)
$bursted = Wait-Db "SELECT COUNT(*) FROM entries WHERE kind=1" "102" 10   # 1 张 OCR 图 + 1 张 4K + 100
$peakMB = [math]::Round($p.WorkingSet64/1MB,1)
# 等队列消化与回落（OCR 每张数百 ms，最多 64 张入队）
Start-Sleep -Seconds 12
$p.Refresh(); $settleMB = [math]::Round($p.WorkingSet64/1MB,1)
$countDb = sqlite3 $db "SELECT COUNT(*) FROM entries WHERE kind=1;"
Write-Host "=== [G] 100-burst: ${burstSec}s, stored=$countDb (expect 102), peak=${peakMB}MB, settled=${settleMB}MB"
if (-not $bursted) { Fail "not all 102 images stored (got $countDb)" }
if ($settleMB -gt 35) { Fail "settled WS ${settleMB}MB > 35MB" }

# ---------- 收尾 ----------
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
Write-Host ""
if ($script:pass) { Write-Host "M2 ACCEPTANCE: PASS" -ForegroundColor Green }
else { Write-Host "M2 ACCEPTANCE: FAIL" -ForegroundColor Red; exit 1 }

