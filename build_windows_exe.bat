@echo off
setlocal EnableExtensions
cd /d "%~dp0"

set "PACKAGE_MODE=%~1"
if not defined PACKAGE_MODE set "PACKAGE_MODE=onedir"
if /I "%PACKAGE_MODE%"=="dir" set "PACKAGE_MODE=onedir"
if /I "%PACKAGE_MODE%"=="folder" set "PACKAGE_MODE=onedir"
if /I "%PACKAGE_MODE%"=="single" set "PACKAGE_MODE=onefile"

if /I not "%PACKAGE_MODE%"=="onedir" if /I not "%PACKAGE_MODE%"=="onefile" (
    echo [ERROR] Invalid package mode: %PACKAGE_MODE%
    echo [ERROR] Usage: build_windows_exe.bat [onedir^|onefile]
    exit /b 1
)

set "PYTHON_EXE="
if exist ".venv-package\Scripts\python.exe" set "PYTHON_EXE=.venv-package\Scripts\python.exe"
if not defined PYTHON_EXE if exist ".venv-build\Scripts\python.exe" set "PYTHON_EXE=.venv-build\Scripts\python.exe"
if not defined PYTHON_EXE if exist ".venv\Scripts\python.exe" set "PYTHON_EXE=.venv\Scripts\python.exe"
if not defined PYTHON_EXE if exist "venv\Scripts\python.exe" set "PYTHON_EXE=venv\Scripts\python.exe"

if not defined PYTHON_EXE (
    echo [ERROR] Could not find .venv-package, .venv-build, .venv, or venv Python
    echo [ERROR] Create a virtual environment and install requirements-build.txt first.
    exit /b 1
)

"%PYTHON_EXE%" -c "import sys; print('[INFO] Python runtime:', sys.version.split()[0]); raise SystemExit(0 if sys.version_info[:2] == (3, 11) else 1)"
if errorlevel 1 (
    echo [ERROR] Windows EXE packaging requires Python 3.11.
    exit /b 1
)

if not defined VIPS_HOME if exist "%cd%\vips" set "VIPS_HOME=%cd%\vips"
if not defined VIPS_HOME if exist "%cd%\third_party\vips" set "VIPS_HOME=%cd%\third_party\vips"
if not defined VIPS_HOME if exist "C:\vips" set "VIPS_HOME=C:\vips"
if not defined VIPS_HOME for /d %%D in ("C:\Program Files\vips*") do if not defined VIPS_HOME set "VIPS_HOME=%%~fD"
if not defined VIPS_HOME for /d %%D in ("C:\Program Files\libvips*") do if not defined VIPS_HOME set "VIPS_HOME=%%~fD"

if defined VIPS_HOME (
    echo [INFO] VIPS_HOME=%VIPS_HOME%
) else (
    echo [WARN] VIPS_HOME is not set. NDPI/MRXS support may be unavailable on target machines.
)

echo [INFO] Using Python: %PYTHON_EXE%
echo [INFO] Package mode: %PACKAGE_MODE%

if /I "%PACKAGE_MODE%"=="onedir" (
    set "OUTPUT_TARGET=dist\PathologySVSConverter\PathologySVSConverter.exe"
) else (
    set "OUTPUT_TARGET=dist\PathologySVSConverter.exe"
)

set "SVS_PACKAGE_MODE=%PACKAGE_MODE%"

if exist "dist\PathologySVSConverter" (
    echo [INFO] Removing previous dist\PathologySVSConverter directory
    rmdir /s /q "dist\PathologySVSConverter"
)

if exist "dist\PathologySVSConverter.exe" (
    echo [INFO] Removing previous dist\PathologySVSConverter.exe
    del /f /q "dist\PathologySVSConverter.exe"
)

if exist "dist\RCX*.tmp" (
    echo [INFO] Removing previous PyInstaller temp files from dist
    del /f /q "dist\RCX*.tmp"
)

if defined UPX_DIR (
    echo [INFO] UPX_DIR=%UPX_DIR%
    "%PYTHON_EXE%" -m PyInstaller --noconfirm --clean --upx-dir "%UPX_DIR%" svs_converter_gui.spec
) else (
    echo [INFO] UPX_DIR is not set. Building without an explicit UPX path.
    "%PYTHON_EXE%" -m PyInstaller --noconfirm --clean svs_converter_gui.spec
)

if errorlevel 1 (
    echo [ERROR] PyInstaller build failed.
    exit /b %errorlevel%
)

echo.
echo [OK] Build completed: %OUTPUT_TARGET%
endlocal
