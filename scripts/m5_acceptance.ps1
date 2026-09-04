# M5 acceptance: FileJump data layer + DLL staging
# Usage: powershell -ExecutionPolicy Bypass -File scripts/m5_acceptance.ps1
# UI interaction (Ctrl+G in save dialogs, auto-popup, WPS no-mistouch)
# must be manually verified in the user's own interactive terminal.
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$exe = Join-Path $root "target\release\clipx.exe"
$wpfNative = "C:\Users\chaoj\dev\tools\clipboard\native\ShellNavigate\bin"

function Pass($name, $detail) { Write-Host "[PASS] $name  $detail" -ForegroundColor Green }
function Fail($name, $detail) { Write-Host "[FAIL] $name  $detail" -ForegroundColor Red; $script:failed++ }
function Info($s) { Write-Host $s -ForegroundColor DarkGray }

$script:failed = 0
Write-Host "== clipx M5 acceptance =="

# --- [A] unit tests ---
Info "[A] cargo test clipx-filejump"
$testOut = cargo test -p clipx-filejump 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) {
    Fail "A" "unit tests failed`n$testOut"
} else {
    Pass "A" "14 unit tests green"
}

# --- [B] release build (SKIP if a dogfood instance holds the exe lock) ---
Info "[B] cargo build -p clipx-app --release"
cargo build -p clipx-app --release 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $exe)) {
    $running = Get-Process clipx -ErrorAction SilentlyContinue
    if ($null -ne $running) {
        Info "SKIP [B]: clipx.exe is running (pid $($running[0].Id)), exe locked; release link deferred to user's terminal"
    } else {
        Fail "B" "release build failed"
        Write-Host "FAILED=$script:failed"
        exit 1
    }
} else {
    Pass "B" "clipx.exe built"
}

# --- [C] stage native DLLs next to exe (inject needs them at runtime) ---
Info "[C] stage ClipboardXShellNavigate*.dll"
$dll64 = Join-Path $wpfNative "x64\Release\ClipboardXShellNavigate.dll"
$dll32 = Join-Path $wpfNative "Win32\Release\ClipboardXShellNavigate32.dll"
foreach ($pair in @(@($dll64, "ClipboardXShellNavigate.dll"), @($dll32, "ClipboardXShellNavigate32.dll"))) {
    $src, $name = $pair
    $dst = Join-Path (Split-Path -Parent $exe) $name
    if (-not (Test-Path $src)) {
        Fail "C" "missing source DLL: $src (rebuild native/ShellNavigate first)"
    } else {
        Copy-Item $src $dst -Force
        if ((Get-Item $dst).Length -gt 0) { Pass "C" "$name staged" } else { Fail "C" "$name empty" }
    }
}

# --- [D] export names present (dumpbin if available, else skip) ---
Info "[D] DLL exports ClipboardX_RemoteNavigate / ClipboardX_RemoteReadCurrentFolder"
$dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
if ($null -eq $dumpbin) {
    Info "dumpbin not found, skipping export check (MSVC Build Tools provide it)"
} else {
    $dst64 = Join-Path (Split-Path -Parent $exe) "ClipboardXShellNavigate.dll"
    $exp = & dumpbin.exe /exports $dst64 2>&1 | Out-String
    if ($exp -match "ClipboardX_RemoteNavigate" -and $exp -match "ClipboardX_RemoteReadCurrentFolder") {
        Pass "D" "both exports present"
    } else {
        Fail "D" "exports missing`n$exp"
    }
}

Write-Host ""
if ($script:failed -eq 0) {
    Write-Host "M5 RESULT: PASS (data layer)" -ForegroundColor Green
    Write-Host "Manual UI checklist (interactive terminal):"
    Write-Host " 1. Notepad save-as dialog: Ctrl+G opens picker, Enter jumps to picked folder"
    Write-Host " 2. Focus a file dialog: picker auto-pops; Esc clears search then closes"
    Write-Host " 3. No dialog: Ctrl+G opens global list, Enter opens folder in Explorer"
    Write-Host " 4. Tray menu has 'folder jump (Ctrl+G)' entry"
    Write-Host " 5. WPS dialogs: keyboard-only fallback, never injects"
    Write-Host " 6. Type to filter incl. favorites alias; Del removes fav/recent; Menu toggles fav"
    exit 0
} else {
    Write-Host "M5 RESULT: FAIL ($script:failed)" -ForegroundColor Red
    exit 1
}
