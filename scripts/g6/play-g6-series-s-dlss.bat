@echo off
setlocal EnableExtensions
rem ---------------------------------------------------------------------------
rem  G6 on VIctory, Series S profile + NVIDIA DLSS: the g6-vt2 branch's best
rem  setup (about 16-17 FPS on an RTX 4070 SUPER / Ryzen 9 5950X; Series X
rem  without DLSS is about 13). Needs an NVIDIA RTX card.
rem
rem  Usage (double-click asks for the folder):
rem    play-g6-series-s-dlss.bat "D:\Games\G6" [quality^|balanced^|performance^|ultra-performance]
rem
rem  - The game folder is your own dump: it holds game_scarlett_master.exe.
rem  - DLSS renders smaller and upscales to 1920x1080: quality 1280x720
rem    (default), balanced, performance 960x540, ultra-performance 640x360.
rem  - nvngx_dlss.dll: put NVIDIA's DLL (NVIDIA DLSS SDK on GitHub,
rem    github.com/NVIDIA/DLSS, folder lib\Windows_x86_64\rel) in the game
rem    folder or next to victory.exe. Without it the driver's copy is used,
rem    which some drivers do not start. If the NVIDIA App's DLSS override is
rem    on, the App's copy runs either way.
rem  - F10 shows/hides the panel (its "DLSS QUALITY ON n/S" line says DLSS is
rem    working). F8 saves a picture. Logs: %LOCALAPPDATA%\victory-g6\events.txt
rem  - The first run builds shader pipelines and stutters; later runs reuse them.
rem ---------------------------------------------------------------------------

set "GAME=%~1"
if "%GAME%"=="" set /p "GAME=Folder of your G6 dump (holds game_scarlett_master.exe): "
set "GAME=%GAME:"=%"
if "%GAME:~-1%"=="\" set "GAME=%GAME:~0,-1%"
if not exist "%GAME%\game_scarlett_master.exe" (
    echo.
    echo game_scarlett_master.exe is not in "%GAME%".
    echo Point this at the folder of your own G6 dump.
    pause
    exit /b 1
)

set "PRESET=%~2"
if "%PRESET%"=="" set "PRESET=quality"

rem victory.exe: next to this file (release zip), or the diagnostics build of a checkout.
set "EXE=%~dp0..\..\victory.exe"
if not exist "%EXE%" set "EXE=%~dp0victory.exe"
if not exist "%EXE%" set "EXE=%~dp0..\..\target\diag\victory.exe"
if not exist "%EXE%" (
    echo.
    echo victory.exe not found. Unpack the release zip keeping its folders, or build it:
    echo   cargo build --profile diag -p victory --features diagnostics
    pause
    exit /b 1
)
for %%I in ("%EXE%") do set "EXE=%%~fI"

rem Where nvngx_dlss.dll is: the game folder, else next to victory.exe.
set "DLSSDIR=%GAME%"
if not exist "%DLSSDIR%\nvngx_dlss.dll" for %%I in ("%EXE%") do set "DLSSDIR=%%~dpI"
if "%DLSSDIR:~-1%"=="\" set "DLSSDIR=%DLSSDIR:~0,-1%"
if not exist "%DLSSDIR%\nvngx_dlss.dll" (
    echo.
    echo Note: no nvngx_dlss.dll in the game folder or next to victory.exe.
    echo DLSS will use the driver's copy. If the game does not start, put NVIDIA's
    echo nvngx_dlss.dll in "%GAME%" and run this again.
    echo.
)

echo Starting G6: Series S profile, DLSS %PRESET%, with %EXE%
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-g6.ps1" -GameDir "%GAME%" -Exe "%EXE%" -Console series-s -Extra "VICTORY_DLSS=%PRESET%;VICTORY_DLSS_DIR=%DLSSDIR%"
if errorlevel 1 (
    echo.
    echo The game ended with an error. The log is %LOCALAPPDATA%\victory-g6\events.txt
    pause
)
endlocal
