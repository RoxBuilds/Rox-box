@echo off
rem Starts play.ps1 from a double-click or a command prompt. PowerShell's
rem execution policy is bypassed for this one process only: nothing on the
rem PC is changed. Everything after the name is passed on:  play.cmd -Check
rem Options that take several values take them as text here:
rem   play.cmd -More "VICTORY_AMBIENT=0.8;VICTORY_WINDOW_MAX=1920x1080"
setlocal
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0play.ps1" %*
set "code=%ERRORLEVEL%"
rem Double-clicked (no arguments, a window of its own): keep what was said on screen.
if "%~1"=="" (
    echo %cmdcmdline% | findstr /i /c:"%~nx0" >nul 2>&1 && pause
)
exit /b %code%
