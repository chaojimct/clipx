@echo off
rem ============================================================================
rem  clipx TSF 光标信标 —— HKLM 注册脚本
rem
rem  用途：把 clipx_caret_beacon.dll 注册成键盘类 TSF TIP，
rem        使其被 TSF 注入到所有宿主进程（Electron / 原生控件 …）。
rem
rem  **必须以管理员身份运行**（HKLM + 机器级证书信任）。
rem  用法：右键本文件 ->「以管理员身份运行」
rem
rem  参数（可选）：
rem    register_beacon_hklm.bat <dll 绝对路径>
rem  默认取 <仓库根>\target\release\clipx_caret_beacon.dll
rem ============================================================================
setlocal enabledelayedexpansion

set TIP={C4BE0C0A-9326-4C1A-BEAC-0CBEAC0DEC01}
set PROF={C4BE0C0A-9326-4C1A-BEAC-0CBEAC0DEC02}
set CER_NAME=clipx Caret Beacon

rem --- 定位仓库根（本脚本在 <repo>\scripts\ 下） ---
set SCRIPT_DIR=%~dp0
for %%I in ("%SCRIPT_DIR%..") do set REPO=%%~fI

if "%~1"=="" (
  set DLL=%REPO%\target\release\clipx_caret_beacon.dll
) else (
  set DLL=%~1
)

set LOG=%TEMP%\clipx_beacon_register.log
echo register start > "%LOG%"

rem --- 权限自检 ---
net session >nul 2>&1
if errorlevel 1 (
  echo [FAIL] 需要管理员权限。请右键本文件 -> 以管理员身份运行。
  echo [FAIL] 需要管理员权限。请右键本文件 -> 以管理员身份运行。>> "%LOG%"
  echo Log: %LOG%
  pause
  exit /b 1
)

if not exist "%DLL%" (
  echo [FAIL] 找不到 DLL: %DLL%
  echo [FAIL] 找不到 DLL: %DLL%>> "%LOG%"
  echo 先跑: cargo build -p clipx-tsf --release
  echo Log: %LOG%
  pause
  exit /b 1
)

echo DLL: %DLL%>> "%LOG%"

rem --- 1. 代码签名 + 证书信任 -------------------------------------------------
rem 第三方 IME/TIP 必须签名，否则 TSF 拒绝加载。
rem 自签证书 + 机器级 Root/TrustedPublisher 信任即可（内部分发够用，
rem 正式对外分发需换成受信任 CA 的代码签名证书）。
set CER=%SCRIPT_DIR%clipx_beacon_cert.cer
if not exist "%CER%" (
  echo [1/4] 生成自签代码签名证书...
  echo [1/4] 生成自签代码签名证书...>> "%LOG%"
  powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%make_beacon_cert.ps1" >> "%LOG%" 2>&1
  if errorlevel 1 (
    echo [FAIL] 证书生成失败，见 %LOG%
    pause
    exit /b 1
  )
) else (
  echo [1/4] 复用已有证书 clipx_beacon_cert.cer
)

certutil -addstore Root "%CER%" >> "%LOG%" 2>&1
certutil -addstore TrustedPublisher "%CER%" >> "%LOG%" 2>&1

rem --- 2. COM 类注册 ----------------------------------------------------------
echo [2/4] 注册 CLSID...
reg add "HKLM\SOFTWARE\Classes\CLSID\%TIP%" /ve /d "clipx Caret Beacon" /f >> "%LOG%" 2>&1
reg add "HKLM\SOFTWARE\Classes\CLSID\%TIP%\InProcServer32" /ve /d "%DLL%" /f >> "%LOG%" 2>&1
reg add "HKLM\SOFTWARE\Classes\CLSID\%TIP%\InProcServer32" /v ThreadingModel /d Apartment /f >> "%LOG%" 2>&1

rem --- 3. TIP 注册 + 键盘类 category -----------------------------------------
echo [3/4] 注册 TIP + 键盘 category...
reg add "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%" /v Enable /t REG_SZ /d 1 /f >> "%LOG%" 2>&1

rem 键盘类 TIP 必须挂到这 7 个 category 下，否则 Win+Space 看不到 / 无法激活。
for %%C in (
  {046B8C80-1647-40F7-9B21-B93B81AABC1B}
  {13A016DF-560B-46CD-947A-4C3AF1E0E35D}
  {25504FB4-7BAB-4BC1-9C69-CF81890F0EF5}
  {34745C63-B2F0-4784-8B67-5E12C8701A31}
  {364215D9-75BC-11D7-A6EF-00065B84435C}
  {49D2F9CF-1F5E-11D7-A6D3-00065B84435C}
  {CCF05DD7-4A87-11D7-A6E2-00065B84435C}
) do (
  reg add "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%\Category\Category\%%C\%TIP%" /f >> "%LOG%" 2>&1
  reg add "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%\Category\Item\%TIP%\%%C" /f >> "%LOG%" 2>&1
)

rem --- 4. 语言 profile（简体中文 0x0804）--------------------------------------
echo [4/4] 注册语言 profile 0x0804...
reg add "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%\LanguageProfile\0x00000804\%PROF%" /v Description /d "clipx 光标信标" /f >> "%LOG%" 2>&1
reg add "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%\LanguageProfile\0x00000804\%PROF%" /v IconFile /d "%DLL%" /f >> "%LOG%" 2>&1
reg add "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%\LanguageProfile\0x00000804\%PROF%" /v IconIndex /t REG_DWORD /d 0 /f >> "%LOG%" 2>&1
reg add "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%\LanguageProfile\0x00000804\%PROF%" /v "Display Description" /d "clipx 光标信标" /f >> "%LOG%" 2>&1
reg add "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%\LanguageProfile\0x00000804\%PROF%" /v Enable /t REG_DWORD /d 1 /f >> "%LOG%" 2>&1

echo register done >> "%LOG%"
echo.
echo [OK] 注册完成。
echo.
echo   DLL   : %DLL%
echo   Log   : %LOG%
echo.
echo 注意：
echo   1. 新启动的进程才会加载信标 DLL（已运行的进程需重启）。
echo   2. 信标不抢占活动输入法，用户的微信/搜狗等照常使用。
echo   3. 卸载请跑 scripts\unregister_beacon_hklm.bat
echo.
pause
exit /b 0
