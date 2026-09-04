# M4 验收：Everything IPC + Explorer 快速查找数据层
# 用法：powershell -ExecutionPolicy Bypass -File scripts/m4_acceptance.ps1
# UI 交互（Explorer 内打字呼出）须在用户终端手动点验。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$exe = Join-Path $root "target\release\clipx.exe"

function Pass($name, $detail) { Write-Host "[PASS] $name  $detail" -ForegroundColor Green }
function Fail($name, $detail) { Write-Host "[FAIL] $name  $detail" -ForegroundColor Red; $script:failed++ }
function Info($s) { Write-Host $s -ForegroundColor DarkGray }

$script:failed = 0

Write-Host "== clipx M4 验收 =="

# --- [A] 单测 ---
Info "[A] cargo test clipx-everything + explorer_quickfind"
$testOut = cargo test -p clipx-everything -p clipx-app -- --test-threads=1 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) {
    Fail "A" "单测失败`n$testOut"
} else {
    Pass "A" "workspace 相关单测通过"
}

# --- 构建 release（--everything-query 入口）---
Info "cargo build -p clipx-app --release"
cargo build -p clipx-app --release | Out-Null
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $exe)) {
    Fail "build" "release 构建失败"
    Write-Host "FAILED=$script:failed"
    exit 1
}

function Invoke-EverythingQuery([string]$expr) {
    $outFile = Join-Path $env:TEMP "clipx-m4-query.txt"
    $env:CLIPX_QUERY_OUT = $outFile
    if (Test-Path $outFile) { Remove-Item $outFile -Force }
    $code = 0
    try {
        & $exe --everything-query $expr | Out-Null
        $code = $LASTEXITCODE
    } finally {
        Remove-Item Env:CLIPX_QUERY_OUT -ErrorAction SilentlyContinue
    }
    $text = if (Test-Path $outFile) { Get-Content $outFile -Raw } else { "" }
    if ($code -ne 0 -and $text -eq "") { $text = "exit=$code" }
    return $text
}

# --- [B] 关键词查询 ---
Info "[B] --everything-query windows"
$b = Invoke-EverythingQuery "windows"
if ($b -match "query failed: Everything 未运行") {
    Fail "B" "IPC 窗口不可达（服务在 session 0 且用户态客户端未拉起）`n$b"
} elseif ($b -match "items=(\d+)") {
    $n = [int]$Matches[1]
    if ($n -gt 0) { Pass "B" ("items={0}" -f $n) } else { Fail "B" "0 条`n$b" }
} else {
    Fail "B" $b
}

# --- [C] parent: 一层 ---
Info "[C] parent:C:\Windows system32"
$c = Invoke-EverythingQuery "parent:C:\Windows system32"
if ($c -match "query failed: Everything 未运行") {
    Fail "C" "Everything 未运行`n$c"
} elseif ($c -match "system32") {
    Pass "C" "parent: 命中 System32"
} else {
    Fail "C" $c
}

# --- [D] path: 树下 ---
Info "[D] path:C:\Windows notepad"
$d = Invoke-EverythingQuery "path:C:\Windows notepad"
if ($d -match "query failed: Everything 未运行") {
    Fail "D" "Everything 未运行`n$d"
} elseif ($d -match "items=(\d+)") {
    $n = [int]$Matches[1]
    if ($n -gt 0) { Pass "D" ("items={0}" -f $n) } else { Fail "D" "0 条`n$d" }
} else {
    Fail "D" $d
}

Write-Host ""
if ($script:failed -eq 0) {
    Write-Host "M4 RESULT: PASS" -ForegroundColor Green
    Write-Host "余下手动：资源管理器文件区打字 → 浮层、↑↓/Enter 定位选中、Esc 关闭、Everything 未运行时仅当前文件夹列表。"
    exit 0
} else {
    Write-Host "M4 RESULT: FAIL ($script:failed)" -ForegroundColor Red
    exit 1
}
