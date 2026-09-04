@echo off
setlocal

for %%I in ("%~dp0..") do set "MEETILY_ROOT=%%~fI"
for %%V in ("%MEETILY_ROOT%") do if /i "%%~dV"=="C:" (
    echo Meetily portable workspace must not run from the C drive:
    echo   "%MEETILY_ROOT%"
    exit /b 1
)
set "MEETILY_PORTABLE=%MEETILY_ROOT%\portable"

if not exist "%MEETILY_PORTABLE%\cargo\bin\cargo.exe" (
    echo Portable Meetily toolchain not found: "%MEETILY_PORTABLE%"
    exit /b 1
)

set "MEETILY_DATA_DIR=%MEETILY_PORTABLE%\app-data"
set "MEETILY_PROCESS_TEMP=%MEETILY_DATA_DIR%\temp"
if defined MEETILY_SESSION_TEMP (
    for %%V in ("%MEETILY_SESSION_TEMP%") do set "MEETILY_PROCESS_TEMP=%%~fV"
    for %%V in ("%MEETILY_SESSION_TEMP%") do if /i "%%~dV"=="C:" (
        echo Meetily development temporary directory must not use the C drive:
        echo   "%%~fV"
        exit /b 1
    )
)
for %%D in (
    "%MEETILY_PROCESS_TEMP%"
    "%MEETILY_DATA_DIR%\webview"
    "%MEETILY_DATA_DIR%\cache\models\huggingface"
    "%MEETILY_DATA_DIR%\cache\python-pycache"
    "%MEETILY_DATA_DIR%\cache\numba"
    "%MEETILY_DATA_DIR%\cache\torch-inductor"
    "%MEETILY_DATA_DIR%\cache\torch-extensions"
    "%MEETILY_DATA_DIR%\cache\triton"
    "%MEETILY_DATA_DIR%\config\matplotlib"
    "%MEETILY_PORTABLE%\conda-envs"
    "%MEETILY_PORTABLE%\conda-pkgs"
    "%MEETILY_PORTABLE%\mamba"
    "%MEETILY_PORTABLE%\python-userbase"
    "%MEETILY_PORTABLE%\sccache"
) do if not exist "%%~D" mkdir "%%~D"
set "TEMP=%MEETILY_PROCESS_TEMP%"
set "TMP=%MEETILY_PROCESS_TEMP%"
set "CARGO_HOME=%MEETILY_PORTABLE%\cargo"
set "RUSTUP_HOME=%MEETILY_PORTABLE%\rustup"
set "CARGO_TARGET_DIR=%MEETILY_PORTABLE%\cargo-target"
set "CARGO_INSTALL_ROOT=%MEETILY_PORTABLE%\cargo-install"
set "NPM_CONFIG_CACHE=%MEETILY_PORTABLE%\pnpm-cache"
set "NPM_CONFIG_USERCONFIG=%MEETILY_PORTABLE%\tool-config\npmrc"
set "NPM_CONFIG_PREFIX=%MEETILY_PORTABLE%\npm-global"
set "npm_config_store_dir=%MEETILY_PORTABLE%\pnpm-store"
set "npm_config_devdir=%MEETILY_PORTABLE%\node-gyp"
set "PNPM_CONFIG_STORE_DIR=%MEETILY_PORTABLE%\pnpm-store"
set "PNPM_CONFIG_CACHE_DIR=%MEETILY_PORTABLE%\pnpm-cache"
set "PNPM_CONFIG_STATE_DIR=%MEETILY_PORTABLE%\pnpm-state"
set "PNPM_CONFIG_GLOBAL_DIR=%MEETILY_PORTABLE%\pnpm-global"
set "PNPM_CONFIG_GLOBAL_BIN_DIR=%MEETILY_PORTABLE%\pnpm-home"
set "PNPM_HOME=%MEETILY_PORTABLE%\pnpm-home"
set "COREPACK_HOME=%MEETILY_PORTABLE%\corepack"
set "XDG_CACHE_HOME=%MEETILY_PORTABLE%\tool-cache"
set "XDG_CONFIG_HOME=%MEETILY_PORTABLE%\tool-config"
set "XDG_DATA_HOME=%MEETILY_PORTABLE%\tool-data"
set "XDG_STATE_HOME=%MEETILY_PORTABLE%\tool-state"
set "PIP_CACHE_DIR=%MEETILY_PORTABLE%\pip-cache"
set "UV_CACHE_DIR=%MEETILY_PORTABLE%\uv-cache"
set "UV_PYTHON_INSTALL_DIR=%MEETILY_PORTABLE%\uv-python"
set "UV_PYTHON_BIN_DIR=%MEETILY_PORTABLE%\uv-python\bin"
set "UV_TOOL_DIR=%MEETILY_PORTABLE%\uv-tools"
set "UV_TOOL_BIN_DIR=%MEETILY_PORTABLE%\uv-tools\bin"
set "HF_HOME=%MEETILY_DATA_DIR%\cache\models\huggingface"
set "HF_HUB_CACHE=%HF_HOME%\hub"
set "HUGGINGFACE_HUB_CACHE=%HF_HOME%\hub"
set "HF_ASSETS_CACHE=%HF_HOME%\assets"
set "TRANSFORMERS_CACHE=%HF_HOME%\transformers"
set "HF_DATASETS_CACHE=%HF_HOME%\datasets"
set "SENTENCE_TRANSFORMERS_HOME=%MEETILY_DATA_DIR%\cache\models\sentence-transformers"
set "TORCH_HOME=%MEETILY_DATA_DIR%\cache\models\torch"
set "ONNX_HOME=%MEETILY_DATA_DIR%\cache\models\onnx"
set "OLLAMA_MODELS=%MEETILY_DATA_DIR%\models\ollama"
set "CUDA_CACHE_PATH=%MEETILY_DATA_DIR%\cache\cuda"
set "CONDA_ENVS_PATH=%MEETILY_PORTABLE%\conda-envs"
set "CONDA_PKGS_DIRS=%MEETILY_PORTABLE%\conda-pkgs"
set "MAMBA_ROOT_PREFIX=%MEETILY_PORTABLE%\mamba"
set "PYTHONUSERBASE=%MEETILY_PORTABLE%\python-userbase"
set "SCCACHE_DIR=%MEETILY_PORTABLE%\sccache"
set "PYTHONPYCACHEPREFIX=%MEETILY_DATA_DIR%\cache\python-pycache"
set "PYTHONNOUSERSITE=1"
set "PYTHONUTF8=1"
set "NUMBA_CACHE_DIR=%MEETILY_DATA_DIR%\cache\numba"
set "TORCHINDUCTOR_CACHE_DIR=%MEETILY_DATA_DIR%\cache\torch-inductor"
set "TORCH_EXTENSIONS_DIR=%MEETILY_DATA_DIR%\cache\torch-extensions"
set "TRITON_CACHE_DIR=%MEETILY_DATA_DIR%\cache\triton"
set "MPLCONFIGDIR=%MEETILY_DATA_DIR%\config\matplotlib"
set "NEXT_TELEMETRY_DISABLED=1"
set "LIBCLANG_PATH=%MEETILY_PORTABLE%\llvm18\bin"
set "CMAKE_CXX_FLAGS=/utf-8"
set "ORT_LIB_LOCATION=%MEETILY_PORTABLE%\onnxruntime\onnxruntime-win-x64-1.22.0"
set "ORT_PREFER_DYNAMIC_LINK=1"
set "ORT_SKIP_DOWNLOAD=1"
set "WEBVIEW2_USER_DATA_FOLDER=%MEETILY_PORTABLE%\app-data\webview"
set "PATH=%MEETILY_PORTABLE%\pnpm\current;%MEETILY_PORTABLE%\cargo\bin;%ORT_LIB_LOCATION%\lib;%PATH%"

set "PNPM_EXE=%MEETILY_PORTABLE%\pnpm\current\pnpm.exe"
if not exist "%PNPM_EXE%" (
    echo Portable pnpm was not found: "%PNPM_EXE%"
    echo Restore portable\pnpm before development; the script will not use a C-drive fallback.
    exit /b 1
)

call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat" -arch=x64 -host_arch=x64 -vcvars_ver=14.41
if errorlevel 1 exit /b %errorlevel%

cd /d "%MEETILY_ROOT%\frontend"
call "%PNPM_EXE%" run tauri:dev:cpu
