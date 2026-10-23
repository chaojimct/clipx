@echo off
rem ============================================================================
rem  clipx TSF 光标信标 —— 卸载脚本（移除 HKLM 注册 + 证书信任）
rem  必须以管理员身份运行。右键本文件 -> 以管理员身份运行
rem ============================================================================
setlocal

set TIP={C4BE0C0A-9326-4C1A-BEAC-0CBEAC0DEC01}
set CER_NAME=clipx Caret Beacon

set SCRIPT_DIR=%~dp0
set CER=%SCRIPT_DIR%clipx_beacon_cert.cer

set LOG=%TEMP%\clipx_beacon_unregister.log
echo unregister start > "%LOG%"

net session >nul 2>&1
if errorlevel 1 (
  echo [FAIL] 需要管理员权限。
  pause
  exit /b 1
)

echo [1/3] 删除 TIP 注册...
reg delete "HKLM\SOFTWARE\Microsoft\CTF\TIP\%TIP%" /f >> "%LOG%" 2>&1

echo [2/3] 删除 CLSID 注册...
reg delete "HKLM\SOFTWARE\Classes\CLSID\%TIP%" /f >> "%LOG%" 2>&1
reg delete "HKLM\SOFTWARE\WOW6432Node\Classes\CLSID\%TIP%" /f >> "%LOG%" 2>&1
reg delete "HKCU\SOFTWARE\Microsoft\CTF\TIP\%TIP%" /f >> "%LOG%" 2>&1
reg delete "HKCU\SOFTWARE\Classes\CLSID\%TIP%" /f >> "%LOG%" 2>&1
reg delete "HKCU\SOFTWARE\WOW6432Node\Classes\CLSID\%TIP%" /f >> "%LOG%" 2>&1

echo [3/3] 移除证书信任...
certutil -delstore Root "%CER_NAME%" >> "%LOG%" 2>&1
certutil -delstore TrustedPublisher "%CER_NAME%" >> "%LOG%" 2>&1

echo unregister done >> "%LOG%"
echo.
echo [OK] 已卸载。请重启已打开的应用（信标 DLL 仍驻留在旧进程里）。
echo Log: %LOG%
pause
exit /b 0
