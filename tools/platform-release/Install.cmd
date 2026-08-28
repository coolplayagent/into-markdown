@echo off
setlocal
set "interactive=1"
if not "%~1"=="" set "interactive=0"
title Into Markdown Installer
echo Installing Into Markdown. This may take a moment...
echo.
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0Install.ps1" %*
set "exit_code=%ERRORLEVEL%"
echo.
if "%exit_code%"=="0" (
  rem Install.ps1 printed the single user-facing success summary.
) else (
  echo Installation failed with exit code %exit_code%.
  echo Keep this window open and use the error above to retry.
)
echo.
if "%interactive%"=="1" pause
exit /b %exit_code%
