use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager, Runtime};

/// Optional absolute path that keeps Meetily's writable data outside the
/// platform default application-data directory.
pub const DATA_DIR_ENV: &str = "MEETILY_DATA_DIR";

const PORTABLE_DIR_NAME: &str = "portable";
const PORTABLE_RUNTIME_DIR_NAME: &str = "runtime";
const PORTABLE_TARGET_DIR_NAME: &str = "cargo-target";

fn has_name(path: &Path, expected: &str) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn validate_windows_storage_prefix(path: &Path, label: &str) -> Result<(), String> {
    use std::path::{Component, Prefix};

    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)
                if letter.eq_ignore_ascii_case(&b'C') =>
            {
                Err(format!(
                    "{label} must not use the C drive in this portable workspace: {}",
                    path.display()
                ))
            }
            Prefix::DeviceNS(_) | Prefix::Verbatim(_) => Err(format!(
                "{label} must not use a Windows device or volume namespace: {}",
                path.display()
            )),
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

#[cfg(not(target_os = "windows"))]
fn validate_windows_storage_prefix(_path: &Path, _label: &str) -> Result<(), String> {
    Ok(())
}

/// Validate a path both lexically and through its nearest existing ancestor.
///
/// The canonical ancestor check catches a D-drive junction that resolves onto
/// C even when the final directory has not been created yet. Callers that
/// create a directory should validate again afterwards to narrow the race
/// between validation and use.
pub(crate) fn validate_portable_storage_path(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "{label} must be an absolute path, got: {}",
            path.display()
        ));
    }

    validate_windows_storage_prefix(path, label)?;

    if let Some(existing_ancestor) = path.ancestors().find(|candidate| candidate.exists()) {
        let canonical = fs::canonicalize(existing_ancestor).map_err(|error| {
            format!(
                "Failed to validate the existing ancestor of {label} {}: {error}",
                path.display()
            )
        })?;
        validate_windows_storage_prefix(&canonical, label)?;
    }

    Ok(())
}

/// Resolve the repository-owned data directory for binaries that are running
/// from this checkout's portable runtime or portable Cargo target directory.
///
/// Keeping this inference in the binary makes a staged `meetily.exe` safe to
/// launch directly: a missing launcher environment cannot silently send the
/// WebView profile, models, database, or recordings back to the system drive.
fn portable_data_dir_from_executable(executable: &Path) -> Option<PathBuf> {
    let executable_dir = executable.parent()?;

    executable_dir.ancestors().take(6).find_map(|candidate| {
        if !has_name(candidate, PORTABLE_DIR_NAME) {
            return None;
        }

        let first_child = executable_dir
            .strip_prefix(candidate)
            .ok()?
            .components()
            .next()?
            .as_os_str()
            .to_string_lossy();

        if first_child.eq_ignore_ascii_case(PORTABLE_RUNTIME_DIR_NAME)
            || first_child.eq_ignore_ascii_case(PORTABLE_TARGET_DIR_NAME)
        {
            Some(candidate.join("app-data"))
        } else {
            None
        }
    })
}

fn resolve_data_dir(
    configured: Option<OsString>,
    executable: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    if let Some(path) = configured
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        validate_portable_storage_path(&path, DATA_DIR_ENV)?;

        return Ok(Some(path));
    }

    let inferred = executable.and_then(portable_data_dir_from_executable);
    if let Some(path) = inferred.as_deref() {
        validate_portable_storage_path(path, "inferred portable data directory")?;
    }
    Ok(inferred)
}

fn configured_data_dir_result() -> Result<Option<PathBuf>, String> {
    let executable = std::env::current_exe().ok();
    resolve_data_dir(std::env::var_os(DATA_DIR_ENV), executable.as_deref())
}

fn create_storage_dir(path: &Path, label: &str) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "Failed to create Meetily {label} directory {}: {error}",
            path.display()
        )
    })
}

fn set_process_path(name: &str, path: &Path) {
    std::env::set_var(name, path);
}

fn runtime_tool_root(data_dir: &Path) -> PathBuf {
    data_dir
        .parent()
        .filter(|parent| has_name(data_dir, "app-data") && has_name(parent, PORTABLE_DIR_NAME))
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.to_path_buf())
}

fn initialize_runtime_cache_paths(data_dir: &Path) -> Result<(), String> {
    let tool_root = runtime_tool_root(data_dir);
    let cache_dir = data_dir.join("cache");
    let config_dir = data_dir.join("config");
    let runtime_data_dir = data_dir.join("runtime-data");
    let state_dir = data_dir.join("state");
    let model_cache_dir = cache_dir.join("models");
    let huggingface_dir = model_cache_dir.join("huggingface");
    let sentence_transformers_dir = model_cache_dir.join("sentence-transformers");
    let ollama_dir = data_dir.join("models").join("ollama");
    let torch_dir = model_cache_dir.join("torch");
    let onnx_dir = model_cache_dir.join("onnx");
    let cuda_dir = cache_dir.join("cuda");
    let pip_dir = cache_dir.join("pip");
    let uv_dir = cache_dir.join("uv");
    let uv_python_dir = tool_root.join("uv-python");
    let uv_python_bin_dir = uv_python_dir.join("bin");
    let uv_tools_dir = tool_root.join("uv-tools");
    let uv_tool_bin_dir = uv_tools_dir.join("bin");
    let conda_envs_dir = tool_root.join("conda-envs");
    let conda_packages_dir = tool_root.join("conda-pkgs");
    let mamba_dir = tool_root.join("mamba");
    let python_user_base_dir = tool_root.join("python-userbase");
    let sccache_dir = tool_root.join("sccache");
    let python_pycache_dir = cache_dir.join("python-pycache");
    let numba_dir = cache_dir.join("numba");
    let torch_inductor_dir = cache_dir.join("torch-inductor");
    let torch_extensions_dir = cache_dir.join("torch-extensions");
    let triton_dir = cache_dir.join("triton");
    let matplotlib_dir = data_dir.join("config").join("matplotlib");

    for (path, label) in [
        (&cache_dir, "cache"),
        (&config_dir, "config"),
        (&runtime_data_dir, "runtime data"),
        (&state_dir, "state"),
        (&model_cache_dir, "model cache"),
        (&huggingface_dir, "Hugging Face cache"),
        (&sentence_transformers_dir, "Sentence Transformers cache"),
        (&ollama_dir, "Ollama model"),
        (&torch_dir, "Torch cache"),
        (&onnx_dir, "ONNX cache"),
        (&cuda_dir, "CUDA cache"),
        (&pip_dir, "pip cache"),
        (&uv_dir, "uv cache"),
        (&uv_python_dir, "uv Python"),
        (&uv_python_bin_dir, "uv Python binaries"),
        (&uv_tools_dir, "uv tools"),
        (&uv_tool_bin_dir, "uv tool binaries"),
        (&conda_envs_dir, "Conda environments"),
        (&conda_packages_dir, "Conda packages"),
        (&mamba_dir, "Mamba root"),
        (&python_user_base_dir, "Python user base"),
        (&sccache_dir, "sccache"),
        (&python_pycache_dir, "Python bytecode cache"),
        (&numba_dir, "Numba cache"),
        (&torch_inductor_dir, "TorchInductor cache"),
        (&torch_extensions_dir, "Torch extensions cache"),
        (&triton_dir, "Triton cache"),
        (&matplotlib_dir, "Matplotlib config"),
    ] {
        create_storage_dir(path, label)?;
    }

    set_process_path("XDG_CACHE_HOME", &cache_dir);
    set_process_path("XDG_CONFIG_HOME", &config_dir);
    set_process_path("XDG_DATA_HOME", &runtime_data_dir);
    set_process_path("XDG_STATE_HOME", &state_dir);
    set_process_path("HF_HOME", &huggingface_dir);
    set_process_path("HF_HUB_CACHE", &huggingface_dir.join("hub"));
    set_process_path("HUGGINGFACE_HUB_CACHE", &huggingface_dir.join("hub"));
    set_process_path("HF_ASSETS_CACHE", &huggingface_dir.join("assets"));
    set_process_path("TRANSFORMERS_CACHE", &huggingface_dir.join("transformers"));
    set_process_path("HF_DATASETS_CACHE", &huggingface_dir.join("datasets"));
    set_process_path("SENTENCE_TRANSFORMERS_HOME", &sentence_transformers_dir);
    set_process_path("TORCH_HOME", &torch_dir);
    set_process_path("ONNX_HOME", &onnx_dir);
    set_process_path("OLLAMA_MODELS", &ollama_dir);
    set_process_path("CUDA_CACHE_PATH", &cuda_dir);
    set_process_path("PIP_CACHE_DIR", &pip_dir);
    set_process_path("UV_CACHE_DIR", &uv_dir);
    set_process_path("UV_PYTHON_INSTALL_DIR", &uv_python_dir);
    set_process_path("UV_PYTHON_BIN_DIR", &uv_python_bin_dir);
    set_process_path("UV_TOOL_DIR", &uv_tools_dir);
    set_process_path("UV_TOOL_BIN_DIR", &uv_tool_bin_dir);
    set_process_path("CONDA_ENVS_PATH", &conda_envs_dir);
    set_process_path("CONDA_PKGS_DIRS", &conda_packages_dir);
    set_process_path("MAMBA_ROOT_PREFIX", &mamba_dir);
    set_process_path("PYTHONUSERBASE", &python_user_base_dir);
    set_process_path("SCCACHE_DIR", &sccache_dir);
    set_process_path("PYTHONPYCACHEPREFIX", &python_pycache_dir);
    set_process_path("NUMBA_CACHE_DIR", &numba_dir);
    set_process_path("TORCHINDUCTOR_CACHE_DIR", &torch_inductor_dir);
    set_process_path("TORCH_EXTENSIONS_DIR", &torch_extensions_dir);
    set_process_path("TRITON_CACHE_DIR", &triton_dir);
    set_process_path("MPLCONFIGDIR", &matplotlib_dir);
    std::env::set_var("PYTHONNOUSERSITE", "1");
    std::env::set_var("PYTHONUTF8", "1");

    Ok(())
}

pub fn configured_data_dir() -> Option<PathBuf> {
    configured_data_dir_result().unwrap_or_else(|error| panic!("{error}"))
}

/// Installed update packages do not preserve the repository-owned portable
/// layout. The frontend uses this flag to disable installer-based updates
/// whenever Meetily owns an explicit or inferred data directory.
#[tauri::command]
pub fn api_is_portable_mode() -> bool {
    configured_data_dir().is_some()
}

/// Prepare process-wide storage overrides before Tauri creates any windows.
///
/// On Windows, WebView2 otherwise keeps local storage, permissions, and caches
/// in its default user-data folder even when the application database and
/// models have been redirected elsewhere.
pub fn initialize_process_paths() -> Result<(), String> {
    let Some(data_dir) = configured_data_dir_result()? else {
        #[cfg(target_os = "windows")]
        return Err(format!(
            "This Windows workspace build requires an absolute {DATA_DIR_ENV} or the repository portable/runtime layout"
        ));

        #[cfg(not(target_os = "windows"))]
        return Ok(());
    };

    create_storage_dir(&data_dir, "data")?;
    validate_portable_storage_path(&data_dir, DATA_DIR_ENV)?;
    std::env::set_var(DATA_DIR_ENV, &data_dir);

    let temp_dir = data_dir.join("temp");
    create_storage_dir(&temp_dir, "temporary")?;
    initialize_runtime_cache_paths(&data_dir)?;

    #[cfg(target_os = "windows")]
    {
        std::env::set_var("TEMP", &temp_dir);
        std::env::set_var("TMP", &temp_dir);

        let webview_data_dir = data_dir.join("webview");
        create_storage_dir(&webview_data_dir, "WebView2 data")?;
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", webview_data_dir);
    }

    #[cfg(not(target_os = "windows"))]
    std::env::set_var("TMPDIR", temp_dir);

    Ok(())
}

pub fn app_data_dir<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<PathBuf> {
    if let Some(path) = configured_data_dir() {
        Ok(path)
    } else {
        app.path().app_data_dir()
    }
}

pub fn store_path<R: Runtime>(
    app: &AppHandle<R>,
    file_name: impl AsRef<Path>,
) -> tauri::Result<PathBuf> {
    Ok(app_data_dir(app)?.join(file_name))
}

pub fn fallback_data_dir() -> Option<PathBuf> {
    configured_data_dir().or_else(|| {
        dirs::data_dir()
            .or_else(dirs::home_dir)
            .map(|path| path.join("Meetily"))
    })
}

pub fn notification_config_dir() -> Option<PathBuf> {
    configured_data_dir()
        .map(|path| path.join("config"))
        .or_else(|| dirs::config_dir().map(|path| path.join("meetily")))
}

pub fn custom_templates_dir() -> Option<PathBuf> {
    configured_data_dir()
        .map(|path| path.join("templates"))
        .or_else(|| dirs::data_dir().map(|path| path.join("Meetily").join("templates")))
}

/// Tauri may create its platform-default local data directory while building
/// the Windows webview even when WebView2 itself has been redirected. Remove
/// that placeholder only when it is empty; never touch a directory containing
/// data from an installed Meetily instance.
#[cfg(target_os = "windows")]
pub fn remove_empty_default_local_data_dir<R: Runtime>(app: &AppHandle<R>) {
    let Some(configured_dir) = configured_data_dir() else {
        return;
    };

    let Ok(default_dir) = app.path().app_local_data_dir() else {
        return;
    };

    if default_dir == configured_dir || !default_dir.is_dir() {
        return;
    }

    let is_empty = match fs::read_dir(&default_dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) => {
            log::warn!(
                "Failed to inspect default Meetily data directory {}: {}",
                default_dir.display(),
                error
            );
            return;
        }
    };

    if is_empty {
        if let Err(error) = fs::remove_dir(&default_dir) {
            log::warn!(
                "Failed to remove empty default Meetily data directory {}: {}",
                default_dir.display(),
                error
            );
        } else {
            log::info!(
                "Removed empty default Meetily data directory: {}",
                default_dir.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_root() -> PathBuf {
        #[cfg(target_os = "windows")]
        return PathBuf::from(r"<project-root>");

        #[cfg(not(target_os = "windows"))]
        PathBuf::from("/workspace/apps/meetily")
    }

    #[test]
    fn staged_runtime_infers_repository_portable_data() {
        let executable = workspace_root()
            .join("portable")
            .join("runtime")
            .join("meetily.exe");

        assert_eq!(
            portable_data_dir_from_executable(&executable),
            Some(workspace_root().join("portable").join("app-data"))
        );
    }

    #[test]
    fn portable_cargo_target_infers_repository_portable_data() {
        let executable = workspace_root()
            .join("portable")
            .join("cargo-target")
            .join("debug")
            .join("meetily.exe");

        assert_eq!(
            portable_data_dir_from_executable(&executable),
            Some(workspace_root().join("portable").join("app-data"))
        );
    }

    #[test]
    fn unrelated_executable_does_not_claim_portable_storage() {
        #[cfg(target_os = "windows")]
        let executable = Path::new(r"C:\Program Files\Meetily\meetily.exe");

        #[cfg(not(target_os = "windows"))]
        let executable = Path::new("/opt/meetily/meetily");

        assert_eq!(portable_data_dir_from_executable(executable), None);
    }

    #[test]
    fn explicit_absolute_data_directory_has_priority() {
        #[cfg(target_os = "windows")]
        let configured = PathBuf::from(r"D:\MeetilyData");

        #[cfg(not(target_os = "windows"))]
        let configured = PathBuf::from("/srv/meetily-data");

        let executable = workspace_root()
            .join("portable")
            .join("runtime")
            .join("meetily.exe");

        assert_eq!(
            resolve_data_dir(Some(configured.clone().into_os_string()), Some(&executable)),
            Ok(Some(configured))
        );
    }

    #[test]
    fn portable_data_uses_the_adjacent_portable_tool_root() {
        let portable = workspace_root().join("portable");
        assert_eq!(runtime_tool_root(&portable.join("app-data")), portable);
    }

    #[test]
    fn custom_data_keeps_tool_state_inside_the_custom_directory() {
        #[cfg(target_os = "windows")]
        let custom = PathBuf::from(r"D:\MeetilyData");

        #[cfg(not(target_os = "windows"))]
        let custom = PathBuf::from("/srv/meetily-data");

        assert_eq!(runtime_tool_root(&custom), custom);
    }

    #[test]
    fn relative_data_directory_is_rejected_without_fallback() {
        let executable = workspace_root()
            .join("portable")
            .join("runtime")
            .join("meetily.exe");
        let result = resolve_data_dir(Some(OsString::from("relative-data")), Some(&executable));

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be an absolute path"));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn c_drive_data_directory_is_rejected_without_fallback() {
        let result = resolve_data_dir(
            Some(OsString::from(r"C:\MeetilyData")),
            Some(Path::new(r"D:\portable\runtime\meetily.exe")),
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not use the C drive"));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn inferred_c_drive_portable_layout_is_rejected() {
        let result = resolve_data_dir(
            None,
            Some(Path::new(r"C:\Meetily\portable\runtime\meetily.exe")),
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not use the C drive"));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn device_namespace_is_rejected() {
        let result = resolve_data_dir(
            Some(OsString::from(r"\\.\C:\MeetilyData")),
            Some(Path::new(r"D:\portable\runtime\meetily.exe")),
        );

        assert!(result.is_err());
        let message = result.unwrap_err();
        assert!(message.contains("device or volume namespace") || message.contains("C drive"));
    }

    #[test]
    fn installed_windows_path_does_not_infer_portable_storage() {
        #[cfg(target_os = "windows")]
        {
            let executable = Path::new(r"C:\Program Files\Meetily\meetily.exe");
            assert_eq!(resolve_data_dir(None, Some(executable)), Ok(None));
        }
    }
}
