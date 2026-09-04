@echo off
setlocal EnableExtensions DisableDelayedExpansion

rem Stage only the output produced by scripts\run-meetily-build-portable.cmd.
rem That wrapper runs: tauri build --debug --no-bundle --ci
rem and writes a SHA-256 marker bound to the resulting meetily.exe.
for %%I in ("%~dp0..") do set "MEETILY_ROOT=%%~fI"
for %%I in ("%MEETILY_ROOT%") do if /i "%%~dI"=="C:" (
    echo Portable runtime staging is not allowed on the C drive:
    echo   "%MEETILY_ROOT%"
    exit /b 1
)

set "PORTABLE_DIR=%MEETILY_ROOT%\portable"
set "SOURCE_DIR=%PORTABLE_DIR%\cargo-target\debug"
set "RUNTIME_DIR=%PORTABLE_DIR%\runtime"
set "ORT_DIR=%PORTABLE_DIR%\onnxruntime\onnxruntime-win-x64-1.22.0\lib"
set "RESOURCE_DIR=%MEETILY_ROOT%\frontend\src-tauri"
set "BUILD_MARKER=%SOURCE_DIR%\.meetily-tauri-debug-no-bundle.sha256"
set "STAGE_TOKEN=%RANDOM%-%RANDOM%"
set "STAGE_DIR=%PORTABLE_DIR%\runtime.stage-%STAGE_TOKEN%"
set "ROLLBACK_DIR=%PORTABLE_DIR%\runtime.rollback-%STAGE_TOKEN%"
set "FAILED_STAGE_DIR=%PORTABLE_DIR%\runtime.failed-%STAGE_TOKEN%"

if exist "%STAGE_DIR%" (
    echo Refusing to reuse an existing staging directory:
    echo   "%STAGE_DIR%"
    exit /b 1
)
if exist "%ROLLBACK_DIR%" (
    echo Refusing to reuse an existing rollback directory:
    echo   "%ROLLBACK_DIR%"
    exit /b 1
)
if exist "%FAILED_STAGE_DIR%" (
    echo Refusing to reuse an existing failed-stage directory:
    echo   "%FAILED_STAGE_DIR%"
    exit /b 1
)

call :require_file "%SOURCE_DIR%\meetily.exe" "Tauri debug executable"
if errorlevel 1 exit /b 1
call :require_file "%BUILD_MARKER%" "Tauri debug no-bundle build marker"
if errorlevel 1 (
    echo Run scripts\run-meetily-build-portable.cmd to create a verified build input.
    echo Direct cargo builds and unmarked executables are not stageable.
    exit /b 1
)

cmd /d /c mkdir "%STAGE_DIR%"
if errorlevel 1 (
    echo Failed to create same-volume staging directory:
    echo   "%STAGE_DIR%"
    exit /b 1
)
if not exist "%STAGE_DIR%" (
    echo Staging directory was not created:
    echo   "%STAGE_DIR%"
    exit /b 1
)
cmd /d /c mkdir "%STAGE_DIR%\templates"
if errorlevel 1 goto :stage_failed
cmd /d /c mkdir "%STAGE_DIR%\workers"
if errorlevel 1 goto :stage_failed
cmd /d /c mkdir "%STAGE_DIR%\workers\moss_runtime_source"
if errorlevel 1 goto :stage_failed
cmd /d /c mkdir "%STAGE_DIR%\workers\moss_runtime_source\moss_transcribe_diarize"
if errorlevel 1 goto :stage_failed

rem Recompute the executable marker at staging time. fc /b binds this exact
rem executable to the successful supported Tauri invocation in the wrapper.
certutil -hashfile "%SOURCE_DIR%\meetily.exe" SHA256 > "%STAGE_DIR%\.meetily-current.sha256"
if errorlevel 1 (
    echo Failed to hash the Tauri debug executable.
    goto :stage_failed
)
fc /b "%BUILD_MARKER%" "%STAGE_DIR%\.meetily-current.sha256" >nul
if errorlevel 1 (
    echo The Tauri build marker does not match the current executable.
    echo Re-run scripts\run-meetily-build-portable.cmd before staging.
    goto :stage_failed
)
cmd /d /c del /q "%STAGE_DIR%\.meetily-current.sha256" >nul
if errorlevel 1 goto :stage_failed
if exist "%STAGE_DIR%\.meetily-current.sha256" goto :stage_failed

rem The current Parakeet runtime explicitly selects the CPU execution provider.
rem A verified Tauri no-bundle build therefore emits the two ORT DLLs below but
rem does not emit DirectML.dll. Do not carry a stale provider DLL forward from
rem an unrelated cargo target/runtime generation.
for %%F in (meetily.exe ffmpeg.exe llama-helper.exe) do (
    call :copy_verified "%SOURCE_DIR%\%%F" "%STAGE_DIR%\%%F" "%%F"
    if errorlevel 1 goto :stage_failed
)

for %%F in (onnxruntime.dll onnxruntime_providers_shared.dll) do (
    call :verify_same "%SOURCE_DIR%\%%F" "%ORT_DIR%\%%F" "Tauri ORT %%F"
    if errorlevel 1 goto :stage_failed
    call :copy_verified "%SOURCE_DIR%\%%F" "%STAGE_DIR%\%%F" "%%F"
    if errorlevel 1 goto :stage_failed
)

for %%F in (
    daily_standup.json
    project_sync.json
    psychatric_session.json
    retrospective.json
    sales_marketing_client_call.json
    standard_meeting.json
) do (
    call :verify_same "%SOURCE_DIR%\templates\%%F" "%RESOURCE_DIR%\templates\%%F" "Tauri template %%F"
    if errorlevel 1 goto :stage_failed
    call :copy_verified "%SOURCE_DIR%\templates\%%F" "%STAGE_DIR%\templates\%%F" "template %%F"
    if errorlevel 1 goto :stage_failed
)

call :verify_same "%SOURCE_DIR%\workers\moss_worker.py" "%RESOURCE_DIR%\workers\moss_worker.py" "Tauri MOSS worker"
if errorlevel 1 goto :stage_failed
call :copy_verified "%SOURCE_DIR%\workers\moss_worker.py" "%STAGE_DIR%\workers\moss_worker.py" "MOSS worker"
if errorlevel 1 goto :stage_failed

for %%F in (LICENSE meetily-source-manifest.json) do (
    call :verify_same "%SOURCE_DIR%\workers\moss_runtime_source\%%F" "%RESOURCE_DIR%\workers\moss_runtime_source\%%F" "Tauri MOSS source %%F"
    if errorlevel 1 goto :stage_failed
    call :copy_verified "%SOURCE_DIR%\workers\moss_runtime_source\%%F" "%STAGE_DIR%\workers\moss_runtime_source\%%F" "MOSS source %%F"
    if errorlevel 1 goto :stage_failed
)

for %%F in (inference_utils.py transcript_parser.py) do (
    call :verify_same "%SOURCE_DIR%\workers\moss_runtime_source\moss_transcribe_diarize\%%F" "%RESOURCE_DIR%\workers\moss_runtime_source\moss_transcribe_diarize\%%F" "Tauri reviewed helper %%F"
    if errorlevel 1 goto :stage_failed
    call :copy_verified "%SOURCE_DIR%\workers\moss_runtime_source\moss_transcribe_diarize\%%F" "%STAGE_DIR%\workers\moss_runtime_source\moss_transcribe_diarize\%%F" "reviewed helper %%F"
    if errorlevel 1 goto :stage_failed
)

rem The allowlist above intentionally excludes model weights, app data,
rem recordings, caches, and every other portable directory.
set "HAD_RUNTIME=0"
if exist "%RUNTIME_DIR%" set "HAD_RUNTIME=1"
if "%HAD_RUNTIME%"=="1" (
    move "%RUNTIME_DIR%" "%ROLLBACK_DIR%" >nul
    if errorlevel 1 (
        echo Existing runtime could not be moved to rollback; it was left unchanged.
        goto :stage_failed
    )
    if not exist "%ROLLBACK_DIR%" (
        echo Rollback directory was not created; attempting restoration.
        goto :restore_old_runtime
    )
)

move "%STAGE_DIR%" "%RUNTIME_DIR%" >nul
if errorlevel 1 (
    echo Staged runtime could not be activated; attempting restoration.
    goto :restore_old_runtime
)
if not exist "%RUNTIME_DIR%" (
    echo Activated runtime directory is missing; attempting restoration.
    goto :restore_old_runtime
)

if exist "%ROLLBACK_DIR%" (
    cmd /d /c rmdir /s /q "%ROLLBACK_DIR%"
    if errorlevel 1 (
        echo New runtime is active, but exact rollback cleanup failed:
        echo   "%ROLLBACK_DIR%"
        exit /b 2
    )
    if exist "%ROLLBACK_DIR%" (
        echo New runtime is active, but exact rollback cleanup is incomplete:
        echo   "%ROLLBACK_DIR%"
        exit /b 2
    )
)

echo Portable Meetily runtime staged atomically at:
echo   "%RUNTIME_DIR%"
exit /b 0

:restore_old_runtime
if exist "%RUNTIME_DIR%" (
    move "%RUNTIME_DIR%" "%FAILED_STAGE_DIR%" >nul
    if errorlevel 1 goto :rollback_failed
    if not exist "%FAILED_STAGE_DIR%" goto :rollback_failed
)
if exist "%ROLLBACK_DIR%" (
    move "%ROLLBACK_DIR%" "%RUNTIME_DIR%" >nul
    if errorlevel 1 goto :rollback_failed
    if not exist "%RUNTIME_DIR%" goto :rollback_failed
)
if exist "%STAGE_DIR%" (
    cmd /d /c rmdir /s /q "%STAGE_DIR%"
    if errorlevel 1 goto :rollback_failed
    if exist "%STAGE_DIR%" goto :rollback_failed
)
if exist "%FAILED_STAGE_DIR%" (
    cmd /d /c rmdir /s /q "%FAILED_STAGE_DIR%"
    if errorlevel 1 goto :rollback_failed
    if exist "%FAILED_STAGE_DIR%" goto :rollback_failed
)
echo Runtime activation failed; the previous runtime was restored.
exit /b 1

:rollback_failed
echo CRITICAL: automatic rollback could not be completed.
echo Do not delete either of these exact directories before inspection:
echo   current:  "%RUNTIME_DIR%"
echo   rollback: "%ROLLBACK_DIR%"
echo   failed:   "%FAILED_STAGE_DIR%"
exit /b 2

:stage_failed
if exist "%STAGE_DIR%" (
    cmd /d /c rmdir /s /q "%STAGE_DIR%"
    if errorlevel 1 (
        echo Failed to remove exact staging directory:
        echo   "%STAGE_DIR%"
        exit /b 2
    )
    if exist "%STAGE_DIR%" (
        echo Exact staging directory still exists:
        echo   "%STAGE_DIR%"
        exit /b 2
    )
)
echo Runtime staging failed; the existing runtime was left unchanged.
exit /b 1

:require_file
if not exist "%~1" (
    echo Required %~2 is missing:
    echo   "%~1"
    exit /b 1
)
exit /b 0

:verify_same
call :require_file "%~1" "%~3 build resource"
if errorlevel 1 exit /b 1
call :require_file "%~2" "%~3 reviewed source"
if errorlevel 1 exit /b 1
fc /b "%~1" "%~2" >nul
if errorlevel 1 (
    echo %~3 does not match its reviewed source:
    echo   build:  "%~1"
    echo   source: "%~2"
    exit /b 1
)
exit /b 0

:copy_verified
call :require_file "%~1" "%~3 input"
if errorlevel 1 exit /b 1
copy /y "%~1" "%~2" >nul
if errorlevel 1 (
    echo Failed to copy %~3:
    echo   from: "%~1"
    echo   to:   "%~2"
    exit /b 1
)
call :require_file "%~2" "%~3 staged output"
if errorlevel 1 exit /b 1
fc /b "%~1" "%~2" >nul
if errorlevel 1 (
    echo Staged %~3 failed byte-for-byte verification:
    echo   from: "%~1"
    echo   to:   "%~2"
    exit /b 1
)
exit /b 0
