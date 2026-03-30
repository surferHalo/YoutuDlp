@echo off
setlocal

cd /d "%~dp0"

echo [1/4] Checking Cargo...
where cargo >nul 2>nul
if errorlevel 1 (
  echo Cargo was not found in PATH.
  echo Please install Rust and make sure cargo is available.
  echo.
  pause
  exit /b 1
)

echo [2/4] Stopping running bridge processes...
taskkill /F /IM youtudlp_bridge.exe >nul 2>nul

echo [3/4] Building release executable...
cargo build --release
if errorlevel 1 (
  echo.
  echo Build failed.
  echo.
  pause
  exit /b 1
)

set "OUTPUT_EXE=%CD%\target\release\youtudlp_bridge.exe"

echo [4/4] Build completed.
if exist "%OUTPUT_EXE%" (
  echo Output: %OUTPUT_EXE%
) else (
  echo Build finished, but the expected executable was not found.
)

echo.
pause
exit /b 0