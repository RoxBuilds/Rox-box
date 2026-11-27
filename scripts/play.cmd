@echo off
rem Starts play.ps1 from a double-click or a command prompt. PowerShell's
rem execution policy is bypassed for this one process only: nothing on the
rem PC is changed. Everything after the name is passed on:  play.cmd -Check
rem Options that take several values take them as text here:
rem   play.cmd -More "VICTORY_AMBIENT=0.8;VICTORY_WINDOW_MAX=1920x1080"
rem First run: a window asks for the folder with your title files.  play.cmd -Setup asks again.
rem Dropping a folder (or the title's .exe) on this file passes its path as the first argument.
setlocal
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0play.ps1" %*
set "code=%ERRORLEVEL%"
rem Double-clicked or dropped on (a window of its own, no option as the first argument): keep what
rem was said on screen. These lines are not in a ( ) block on purpose: a dropped path may hold ")".
if defined ROXBOX_NO_PAUSE goto :done
set "first=%~1"
if not defined first goto :maybepause
if "%first:~0,1%"=="-" goto :done
:maybepause
echo %cmdcmdline% | findstr /i /c:"%~nx0" >nul 2>&1 && pause
:done
exit /b %code%
