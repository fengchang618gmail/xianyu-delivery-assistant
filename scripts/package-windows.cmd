@echo off
rem Locate MSVC + Windows SDK dynamically, set link env, then run tauri build.
rem Solves: 1) Git Bash GNU link.exe shadowing MSVC link.exe; 2) rustc registry probe failure.
rem NOTE: keep this file ASCII-only. cmd.exe parses .cmd in the ANSI codepage;
rem       UTF-8 Chinese comments get mangled and break command parsing.
setlocal enabledelayedexpansion

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
    echo [error] vswhere not found. Install VS 2022 Build Tools first.
    exit /b 1
)

for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSPATH=%%i"
if not defined VSPATH (
    echo [error] MSVC C++ workload not found via vswhere.
    exit /b 1
)

rem Pick the highest-version MSVC toolset directory
set "MSVCVER="
for /f "delims=" %%d in ('dir /b /ad "%VSPATH%\VC\Tools\MSVC" 2^>nul ^| sort') do set "MSVCVER=%%d"
if not defined MSVCVER (
    echo [error] No MSVC toolset directory under !VSPATH!\VC\Tools\MSVC
    exit /b 1
)
set "MSVCBIN=%VSPATH%\VC\Tools\MSVC\%MSVCVER%\bin\Hostx64\x64"
set "MSVCLIB=%VSPATH%\VC\Tools\MSVC\%MSVCVER%\lib\x64"

rem Windows SDK: pick highest Lib version under KitsRoot10
set "SDKROOT="
for /f "skip=2 tokens=2,*" %%a in ('reg query "HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots" /v KitsRoot10 2^>nul') do set "SDKROOT=%%b"
if not defined SDKROOT (
    echo [error] Windows 10/11 SDK not found in registry.
    exit /b 1
)
set "SDKVER="
for /f "delims=" %%d in ('dir /b /ad "%SDKROOT%Lib" 2^>nul ^| sort') do set "SDKVER=%%d"
if not defined SDKVER (
    echo [error] No SDK Lib version under !SDKROOT!Lib
    exit /b 1
)

set "PATH=%MSVCBIN%;%USERPROFILE%\.cargo\bin;%PATH%"
set "LIB=%MSVCLIB%;%SDKROOT%Lib\%SDKVER%\um\x64;%SDKROOT%Lib\%SDKVER%\ucrt\x64"
set "INCLUDE=%VSPATH%\VC\Tools\MSVC\%MSVCVER%\include;%SDKROOT%Include\%SDKVER%\um;%SDKROOT%Include\%SDKVER%\ucrt;%SDKROOT%Include\%SDKVER%\shared"

echo [info] MSVC %MSVCVER% ^| SDK %SDKVER%
cd /d "%~dp0..\src-tauri"
if not exist Cargo.toml (
    echo [error] Cannot find src-tauri\Cargo.toml
    exit /b 1
)

rem Build the nmhost sidecar binary first, then copy it to the externalBin path
cargo build --release --bin nmhost || exit /b 1
if not exist "binaries" mkdir "binaries"
copy /y "target\release\nmhost.exe" "binaries\nmhost-x86_64-pc-windows-msvc.exe" >nul || exit /b 1

npx tauri build %*
exit /b %ERRORLEVEL%
