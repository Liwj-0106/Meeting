//! Bounded JSONL child-process transport for the MOSS worker protocol.
//!
//! Production callers must opt into explicit executable, script, model, audio,
//! and temporary roots. Paths are canonicalized before spawning, the child
//! inherits only a small environment allowlist, stdout is treated as protocol
//! data, and stderr is drained into a private bounded buffer that is never
//! returned through the supervisor API.

use super::moss_protocol::{MossWorkerRequest, MossWorkerResponse, MOSS_WORKER_SCHEMA_VERSION};
use super::worker_supervisor::{
    MossTransportError, MossTransportErrorCode, MossWorkerHandshake, MossWorkerTransport,
    MossWorkerTransportFactory,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MIN_JSONL_BYTES: usize = 1_024;
const MAX_JSONL_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROTOCOL_SKIPS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MossProcessConfigError {
    #[error("a configured process path is invalid or outside its allowed root")]
    InvalidPath,
    #[error("portable worker paths must not use the Windows system drive")]
    SystemDriveRejected,
    #[error("a configured process limit is invalid")]
    InvalidLimit,
}

#[derive(Clone)]
pub struct MossProcessTransportConfig {
    pub python_executable: PathBuf,
    pub executable_root: PathBuf,
    pub worker_script: PathBuf,
    pub worker_root: PathBuf,
    pub portable_root: PathBuf,
    pub model_root: PathBuf,
    pub audio_root: PathBuf,
    pub temp_root: PathBuf,
    pub model_revision: String,
    pub io_timeout: Duration,
    pub handshake_timeout: Duration,
    pub max_jsonl_bytes: usize,
    pub stdout_queue_capacity: usize,
    pub private_stderr_bytes: usize,
    #[cfg(test)]
    fixture_mode: bool,
    #[cfg(test)]
    allow_external_test_runtime: bool,
}

impl MossProcessTransportConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        python_executable: impl Into<PathBuf>,
        executable_root: impl Into<PathBuf>,
        worker_script: impl Into<PathBuf>,
        worker_root: impl Into<PathBuf>,
        portable_root: impl Into<PathBuf>,
        model_root: impl Into<PathBuf>,
        audio_root: impl Into<PathBuf>,
        temp_root: impl Into<PathBuf>,
        model_revision: impl Into<String>,
    ) -> Self {
        Self {
            python_executable: python_executable.into(),
            executable_root: executable_root.into(),
            worker_script: worker_script.into(),
            worker_root: worker_root.into(),
            portable_root: portable_root.into(),
            model_root: model_root.into(),
            audio_root: audio_root.into(),
            temp_root: temp_root.into(),
            model_revision: model_revision.into(),
            io_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(300),
            max_jsonl_bytes: MAX_JSONL_BYTES,
            stdout_queue_capacity: 4,
            private_stderr_bytes: 16 * 1024,
            #[cfg(test)]
            fixture_mode: false,
            #[cfg(test)]
            allow_external_test_runtime: false,
        }
    }
}

impl fmt::Debug for MossProcessTransportConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossProcessTransportConfig")
            .field("python_executable", &"<redacted>")
            .field("executable_root", &"<redacted>")
            .field("worker_script", &"<redacted>")
            .field("worker_root", &"<redacted>")
            .field("portable_root", &"<redacted>")
            .field("model_root", &"<redacted>")
            .field("audio_root", &"<redacted>")
            .field("temp_root", &"<redacted>")
            .field("model_revision", &self.model_revision)
            .field("io_timeout", &self.io_timeout)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("max_jsonl_bytes", &self.max_jsonl_bytes)
            .field("stdout_queue_capacity", &self.stdout_queue_capacity)
            .field("private_stderr_bytes", &self.private_stderr_bytes)
            .finish()
    }
}

#[derive(Clone)]
pub struct MossJsonlProcessFactory {
    config: Arc<PreparedProcessConfig>,
}

impl MossJsonlProcessFactory {
    pub fn new(config: MossProcessTransportConfig) -> Result<Self, MossProcessConfigError> {
        Ok(Self {
            config: Arc::new(PreparedProcessConfig::prepare(config)?),
        })
    }
}

impl fmt::Debug for MossJsonlProcessFactory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossJsonlProcessFactory")
            .field("config", &self.config.public)
            .finish()
    }
}

struct PreparedProcessConfig {
    public: MossProcessTransportConfig,
    python_executable: PathBuf,
    worker_script: PathBuf,
    portable_root: PathBuf,
    model_root: PathBuf,
    audio_root: PathBuf,
    temp_root: PathBuf,
    worker_root: PathBuf,
    cache_paths: WorkerCachePaths,
}

struct WorkerCachePaths {
    cache_root: PathBuf,
    huggingface: PathBuf,
    huggingface_modules: PathBuf,
    transformers: PathBuf,
    torch: PathBuf,
    torch_inductor: PathBuf,
    torch_extensions: PathBuf,
    numba: PathBuf,
    triton: PathBuf,
    cuda: PathBuf,
    pip: PathBuf,
    profile: PathBuf,
    appdata: PathBuf,
    local_appdata: PathBuf,
    matplotlib: PathBuf,
}

impl PreparedProcessConfig {
    fn prepare(mut config: MossProcessTransportConfig) -> Result<Self, MossProcessConfigError> {
        if config.io_timeout.is_zero()
            || config.handshake_timeout.is_zero()
            || !(MIN_JSONL_BYTES..=MAX_JSONL_BYTES).contains(&config.max_jsonl_bytes)
            || config.stdout_queue_capacity == 0
            || config.stdout_queue_capacity > 64
            || config.private_stderr_bytes > 64 * 1024
            || !safe_revision(&config.model_revision)
        {
            return Err(MossProcessConfigError::InvalidLimit);
        }

        for path in [
            &config.python_executable,
            &config.executable_root,
            &config.worker_script,
            &config.worker_root,
            &config.portable_root,
            &config.model_root,
            &config.audio_root,
            &config.temp_root,
        ] {
            validate_absolute_path(path)?;
        }
        validate_windows_storage_path(&config.portable_root)?;

        let executable_root = canonical_directory(&config.executable_root)?;
        let python_executable = canonical_file_within(&config.python_executable, &executable_root)?;
        let worker_root = canonical_directory(&config.worker_root)?;
        let worker_script = canonical_file_within(&config.worker_script, &worker_root)?;
        if worker_script.extension().and_then(|value| value.to_str()) != Some("py") {
            return Err(MossProcessConfigError::InvalidPath);
        }

        let portable_root = canonical_directory(&config.portable_root)?;
        validate_windows_storage_path(&portable_root)?;
        validate_windows_storage_path(&executable_root)?;
        validate_windows_storage_path(&python_executable)?;
        validate_windows_storage_path(&worker_root)?;
        validate_windows_storage_path(&worker_script)?;

        #[cfg(test)]
        let allow_external_runtime = config.allow_external_test_runtime;
        #[cfg(not(test))]
        let allow_external_runtime = false;
        if !allow_external_runtime
            && (!executable_root.starts_with(&portable_root)
                || !python_executable.starts_with(&portable_root))
        {
            return Err(MossProcessConfigError::InvalidPath);
        }

        let model_root = canonicalize_descendant_allow_missing(&config.model_root, &portable_root)?;
        let audio_root = canonical_directory_within(&config.audio_root, &portable_root)?;
        validate_descendant_allow_missing(&config.temp_root, &portable_root)?;
        std::fs::create_dir_all(&config.temp_root)
            .map_err(|_| MossProcessConfigError::InvalidPath)?;
        let temp_root = canonical_directory_within(&config.temp_root, &portable_root)?;
        let cache_root = portable_root.join("app-data/cache");
        let cache_paths = WorkerCachePaths {
            cache_root: create_canonical_directory_within(&cache_root, &portable_root)?,
            huggingface: create_canonical_directory_within(
                &cache_root.join("huggingface"),
                &portable_root,
            )?,
            huggingface_modules: create_canonical_directory_within(
                &cache_root.join("huggingface/modules"),
                &portable_root,
            )?,
            transformers: create_canonical_directory_within(
                &cache_root.join("huggingface/transformers"),
                &portable_root,
            )?,
            torch: create_canonical_directory_within(&cache_root.join("torch"), &portable_root)?,
            torch_inductor: create_canonical_directory_within(
                &cache_root.join("torch-inductor"),
                &portable_root,
            )?,
            torch_extensions: create_canonical_directory_within(
                &cache_root.join("torch-extensions"),
                &portable_root,
            )?,
            numba: create_canonical_directory_within(&cache_root.join("numba"), &portable_root)?,
            triton: create_canonical_directory_within(&cache_root.join("triton"), &portable_root)?,
            cuda: create_canonical_directory_within(&cache_root.join("cuda"), &portable_root)?,
            pip: create_canonical_directory_within(&cache_root.join("pip"), &portable_root)?,
            profile: create_canonical_directory_within(
                &cache_root.join("moss-runtime-profile"),
                &portable_root,
            )?,
            appdata: create_canonical_directory_within(
                &cache_root.join("moss-runtime-profile/AppData/Roaming"),
                &portable_root,
            )?,
            local_appdata: create_canonical_directory_within(
                &cache_root.join("moss-runtime-profile/AppData/Local"),
                &portable_root,
            )?,
            matplotlib: create_canonical_directory_within(
                &cache_root.join("moss-runtime-profile/matplotlib"),
                &portable_root,
            )?,
        };

        config.python_executable = python_executable.clone();
        config.worker_script = worker_script.clone();
        config.worker_root = worker_root.clone();
        config.portable_root = portable_root.clone();
        config.model_root = model_root.clone();
        config.audio_root = audio_root.clone();
        config.temp_root = temp_root.clone();

        Ok(Self {
            public: config,
            python_executable,
            worker_script,
            portable_root,
            model_root,
            audio_root,
            temp_root,
            worker_root,
            cache_paths,
        })
    }
}

#[async_trait]
impl MossWorkerTransportFactory for MossJsonlProcessFactory {
    async fn spawn(&self) -> Result<Box<dyn MossWorkerTransport>, MossTransportError> {
        let config = Arc::clone(&self.config);
        let mut command = Command::new(&config.python_executable);
        command
            .arg("-I")
            .arg("-u")
            .arg(&config.worker_script)
            .arg("--portable-root")
            .arg(&config.portable_root)
            .arg("--model-root")
            .arg(&config.model_root)
            .arg("--audio-root")
            .arg(&config.audio_root)
            .arg("--model-revision")
            .arg(&config.public.model_revision)
            .current_dir(&config.worker_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_clear()
            .env("TEMP", &config.temp_root)
            .env("TMP", &config.temp_root)
            .env("PYTHONNOUSERSITE", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PYTHONPYCACHEPREFIX", config.temp_root.join("pycache"))
            .env("USERPROFILE", &config.cache_paths.profile)
            .env("HOME", &config.cache_paths.profile)
            .env("APPDATA", &config.cache_paths.appdata)
            .env("LOCALAPPDATA", &config.cache_paths.local_appdata)
            .env("PIP_CACHE_DIR", &config.cache_paths.pip)
            .env("XDG_CACHE_HOME", &config.cache_paths.cache_root)
            .env("MPLCONFIGDIR", &config.cache_paths.matplotlib)
            .env("HF_HUB_OFFLINE", "1")
            .env("TRANSFORMERS_OFFLINE", "1")
            .env("HF_DATASETS_OFFLINE", "1")
            .env("HF_HUB_DISABLE_TELEMETRY", "1")
            .env("HF_HUB_DISABLE_PROGRESS_BARS", "1")
            .env("TOKENIZERS_PARALLELISM", "false")
            .env("TRANSFORMERS_VERBOSITY", "error")
            .env("HF_HOME", &config.cache_paths.huggingface)
            .env("HF_MODULES_CACHE", &config.cache_paths.huggingface_modules)
            .env("TRANSFORMERS_CACHE", &config.cache_paths.transformers)
            .env("TORCH_HOME", &config.cache_paths.torch)
            .env(
                "TORCHINDUCTOR_CACHE_DIR",
                &config.cache_paths.torch_inductor,
            )
            .env("TORCH_EXTENSIONS_DIR", &config.cache_paths.torch_extensions)
            .env("NUMBA_CACHE_DIR", &config.cache_paths.numba)
            .env("TRITON_CACHE_DIR", &config.cache_paths.triton)
            .env("CUDA_CACHE_PATH", &config.cache_paths.cuda);

        #[cfg(windows)]
        {
            let executable_root = config.python_executable.parent().ok_or_else(|| {
                MossTransportError::new(MossTransportErrorCode::SpawnFailed, true)
            })?;
            let runtime_library = executable_root.join("Library/bin");
            let system_binary = std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .map(|root| root.join("System32"))
                .ok_or_else(|| {
                    MossTransportError::new(MossTransportErrorCode::SpawnFailed, true)
                })?;
            let isolated_path =
                std::env::join_paths([executable_root, &runtime_library, &system_binary]).map_err(
                    |_| MossTransportError::new(MossTransportErrorCode::SpawnFailed, true),
                )?;
            command.env("PATH", isolated_path);
        }
        #[cfg(not(windows))]
        copy_environment_if_present(&mut command, "PATH");
        copy_environment_if_present(&mut command, "SystemRoot");
        copy_environment_if_present(&mut command, "WINDIR");
        copy_environment_if_present(&mut command, "COMSPEC");
        copy_environment_if_present(&mut command, "PATHEXT");
        copy_environment_if_present(&mut command, "CUDA_PATH");
        copy_environment_if_present(&mut command, "CUDA_HOME");

        #[cfg(test)]
        if config.public.fixture_mode {
            command.arg("--fixture");
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .spawn()
            .map_err(|_| MossTransportError::new(MossTransportErrorCode::SpawnFailed, true))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| MossTransportError::new(MossTransportErrorCode::SpawnFailed, true))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MossTransportError::new(MossTransportErrorCode::SpawnFailed, true))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| MossTransportError::new(MossTransportErrorCode::SpawnFailed, true))?;

        let alive = Arc::new(AtomicBool::new(true));
        let (stdout_sender, stdout_receiver) = mpsc::channel(config.public.stdout_queue_capacity);
        let stdout_alive = Arc::clone(&alive);
        let max_jsonl_bytes = config.public.max_jsonl_bytes;
        let stdout_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_bounded_line(&mut reader, max_jsonl_bytes).await {
                    Ok(Some(line)) => {
                        if stdout_sender.send(Ok(line)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        stdout_alive.store(false, Ordering::SeqCst);
                        let _ = stdout_sender
                            .send(Err(MossTransportErrorCode::Crashed))
                            .await;
                        break;
                    }
                    Err(ReadLineFailure::TooLong | ReadLineFailure::InvalidTerminator) => {
                        stdout_alive.store(false, Ordering::SeqCst);
                        let _ = stdout_sender
                            .send(Err(MossTransportErrorCode::Protocol))
                            .await;
                        break;
                    }
                    Err(ReadLineFailure::Io) => {
                        stdout_alive.store(false, Ordering::SeqCst);
                        let _ = stdout_sender.send(Err(MossTransportErrorCode::Io)).await;
                        break;
                    }
                }
            }
        });

        let diagnostics = Arc::new(PrivateStderrDiagnostics::new(
            config.public.private_stderr_bytes,
        ));
        let stderr_diagnostics = Arc::clone(&diagnostics);
        let stderr_task = tokio::spawn(async move {
            let mut reader = stderr;
            let mut chunk = [0u8; 1_024];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => stderr_diagnostics.push(&chunk[..count]),
                }
            }
        });

        Ok(Box::new(MossJsonlProcessTransport {
            child,
            stdin,
            stdout: stdout_receiver,
            alive,
            stdout_task,
            stderr_task,
            _private_diagnostics: diagnostics,
            io_timeout: config.public.io_timeout,
            handshake_timeout: config.public.handshake_timeout,
            max_jsonl_bytes: config.public.max_jsonl_bytes,
        }))
    }
}

pub struct MossJsonlProcessTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: mpsc::Receiver<Result<Vec<u8>, MossTransportErrorCode>>,
    alive: Arc<AtomicBool>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    _private_diagnostics: Arc<PrivateStderrDiagnostics>,
    io_timeout: Duration,
    handshake_timeout: Duration,
    max_jsonl_bytes: usize,
}

impl fmt::Debug for MossJsonlProcessTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossJsonlProcessTransport")
            .field("alive", &self.alive.load(Ordering::SeqCst))
            .field("io_timeout", &self.io_timeout)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("max_jsonl_bytes", &self.max_jsonl_bytes)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl MossWorkerTransport for MossJsonlProcessTransport {
    async fn handshake(&mut self) -> Result<MossWorkerHandshake, MossTransportError> {
        self.send(&ClientMessage::Handshake {
            schema: MOSS_WORKER_SCHEMA_VERSION,
        })
        .await?;
        match self.receive_with_timeout(self.handshake_timeout).await? {
            WorkerMessage::Handshake {
                schema,
                status: WorkerAvailability::Ready,
                worker_revision: Some(worker_revision),
                model_revision: Some(model_revision),
                max_in_flight: Some(max_in_flight),
                ..
            } => Ok(MossWorkerHandshake {
                schema,
                worker_revision,
                model_revision,
                max_in_flight,
            }),
            WorkerMessage::Handshake {
                status: WorkerAvailability::ModelNotInstalled,
                ..
            } => Err(MossTransportError::new(
                MossTransportErrorCode::ModelNotInstalled,
                false,
            )),
            WorkerMessage::Handshake {
                status: WorkerAvailability::Unavailable,
                ..
            } => Err(MossTransportError::new(
                MossTransportErrorCode::RuntimeUnavailable,
                false,
            )),
            _ => Err(MossTransportError::new(
                MossTransportErrorCode::Protocol,
                false,
            )),
        }
    }

    async fn execute(
        &mut self,
        request: &MossWorkerRequest,
    ) -> Result<MossWorkerResponse, MossTransportError> {
        self.send(&ClientMessage::Execute { request }).await?;
        for _ in 0..MAX_PROTOCOL_SKIPS {
            match self.receive_execution().await? {
                WorkerMessage::Result { response } if response.job == request.job => {
                    return Ok(response);
                }
                WorkerMessage::Error {
                    job: Some(job),
                    code,
                } if job == request.job => return Err(map_worker_error(code)),
                WorkerMessage::Cancelled { job } if job == request.job => {
                    return Err(MossTransportError::new(
                        MossTransportErrorCode::CancelFailed,
                        false,
                    ));
                }
                WorkerMessage::Result { .. }
                | WorkerMessage::Cancelled { .. }
                | WorkerMessage::Error { .. } => continue,
                _ => {
                    return Err(MossTransportError::new(
                        MossTransportErrorCode::Protocol,
                        false,
                    ));
                }
            }
        }
        Err(MossTransportError::new(
            MossTransportErrorCode::Protocol,
            false,
        ))
    }

    async fn cancel(&mut self, job: &str) -> Result<(), MossTransportError> {
        self.send(&ClientMessage::Cancel { job }).await?;
        for _ in 0..MAX_PROTOCOL_SKIPS {
            match self.receive_control().await? {
                WorkerMessage::Cancelled { job: cancelled } if cancelled == job => return Ok(()),
                WorkerMessage::Result { response } if response.job == job => continue,
                WorkerMessage::Error {
                    job: Some(failed),
                    code,
                } if failed == job => return Err(map_worker_error(code)),
                WorkerMessage::Result { .. }
                | WorkerMessage::Cancelled { .. }
                | WorkerMessage::Error { .. } => continue,
                _ => {
                    return Err(MossTransportError::new(
                        MossTransportErrorCode::Protocol,
                        false,
                    ));
                }
            }
        }
        Err(MossTransportError::new(
            MossTransportErrorCode::CancelFailed,
            true,
        ))
    }

    async fn shutdown(&mut self) -> Result<(), MossTransportError> {
        if !self.alive.load(Ordering::SeqCst) {
            self.reap_or_kill().await;
            return Ok(());
        }
        self.send(&ClientMessage::Shutdown).await?;
        match self.receive_control().await? {
            WorkerMessage::Shutdown {
                status: ShutdownStatus::Ok,
            } => {}
            _ => {
                self.reap_or_kill().await;
                return Err(MossTransportError::new(
                    MossTransportErrorCode::ShutdownFailed,
                    false,
                ));
            }
        }
        match tokio::time::timeout(self.io_timeout, self.child.wait()).await {
            Ok(Ok(_)) => {
                self.alive.store(false, Ordering::SeqCst);
                Ok(())
            }
            Ok(Err(_)) | Err(_) => {
                self.reap_or_kill().await;
                Err(MossTransportError::new(
                    MossTransportErrorCode::ShutdownFailed,
                    false,
                ))
            }
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl MossJsonlProcessTransport {
    async fn send<T: Serialize>(&mut self, message: &T) -> Result<(), MossTransportError> {
        let mut bytes = serde_json::to_vec(message)
            .map_err(|_| MossTransportError::new(MossTransportErrorCode::Protocol, false))?;
        if bytes.len() > self.max_jsonl_bytes
            || bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n'))
        {
            return Err(MossTransportError::new(
                MossTransportErrorCode::Protocol,
                false,
            ));
        }
        bytes.push(b'\n');
        let write = async {
            self.stdin.write_all(&bytes).await?;
            self.stdin.flush().await
        };
        tokio::time::timeout(self.io_timeout, write)
            .await
            .map_err(|_| MossTransportError::new(MossTransportErrorCode::Io, true))?
            .map_err(|_| MossTransportError::new(MossTransportErrorCode::Io, true))
    }

    async fn receive_control(&mut self) -> Result<WorkerMessage, MossTransportError> {
        self.receive_with_timeout(self.io_timeout).await
    }

    async fn receive_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<WorkerMessage, MossTransportError> {
        let received = tokio::time::timeout(timeout, self.stdout.recv())
            .await
            .map_err(|_| MossTransportError::new(MossTransportErrorCode::Io, true))?;
        decode_worker_message(received)
    }

    // The supervisor owns the end-to-end job timeout and drops this future
    // before issuing a tightly bounded cancellation. Applying the short
    // control-plane timeout here would incorrectly kill ordinary inference
    // that takes longer than a handshake or pipe flush.
    async fn receive_execution(&mut self) -> Result<WorkerMessage, MossTransportError> {
        decode_worker_message(self.stdout.recv().await)
    }

    async fn reap_or_kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(self.io_timeout, self.child.wait()).await;
        self.alive.store(false, Ordering::SeqCst);
    }
}

fn decode_worker_message(
    received: Option<Result<Vec<u8>, MossTransportErrorCode>>,
) -> Result<WorkerMessage, MossTransportError> {
    match received {
        Some(Ok(bytes)) => serde_json::from_slice(&bytes)
            .map_err(|_| MossTransportError::new(MossTransportErrorCode::Protocol, false)),
        Some(Err(code)) => Err(MossTransportError::new(
            code,
            !matches!(code, MossTransportErrorCode::Protocol),
        )),
        None => Err(MossTransportError::new(
            MossTransportErrorCode::Crashed,
            true,
        )),
    }
}

impl Drop for MossJsonlProcessTransport {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.child.start_kill();
        self.stdout_task.abort();
        self.stderr_task.abort();
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage<'a> {
    Handshake { schema: u16 },
    Execute { request: &'a MossWorkerRequest },
    Cancel { job: &'a str },
    Shutdown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerMessage {
    Handshake {
        schema: u16,
        status: WorkerAvailability,
        #[serde(default)]
        worker_revision: Option<String>,
        #[serde(default)]
        model_revision: Option<String>,
        #[serde(default)]
        max_in_flight: Option<u8>,
        #[serde(default)]
        #[serde(rename = "error_code")]
        _error_code: Option<WorkerErrorCode>,
        #[serde(default)]
        #[serde(rename = "backend")]
        _backend: Option<String>,
        #[serde(default)]
        #[serde(rename = "device")]
        _device: Option<String>,
        #[serde(default)]
        #[serde(rename = "dtype")]
        _dtype: Option<String>,
    },
    Result {
        response: MossWorkerResponse,
    },
    Cancelled {
        job: String,
    },
    Error {
        #[serde(default)]
        job: Option<String>,
        code: WorkerErrorCode,
    },
    Shutdown {
        status: ShutdownStatus,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkerAvailability {
    Ready,
    ModelNotInstalled,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ShutdownStatus {
    Ok,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkerErrorCode {
    ModelNotInstalled,
    RuntimeUnavailable,
    InvalidRequest,
    InvalidAudioPath,
    JobBusy,
    JobCancelled,
    InferenceFailed,
}

fn map_worker_error(code: WorkerErrorCode) -> MossTransportError {
    match code {
        WorkerErrorCode::ModelNotInstalled => {
            MossTransportError::new(MossTransportErrorCode::ModelNotInstalled, false)
        }
        WorkerErrorCode::RuntimeUnavailable => {
            MossTransportError::new(MossTransportErrorCode::RuntimeUnavailable, false)
        }
        WorkerErrorCode::JobCancelled => {
            MossTransportError::new(MossTransportErrorCode::CancelFailed, false)
        }
        WorkerErrorCode::InvalidRequest | WorkerErrorCode::InvalidAudioPath => {
            MossTransportError::new(MossTransportErrorCode::Protocol, false)
        }
        WorkerErrorCode::JobBusy | WorkerErrorCode::InferenceFailed => {
            MossTransportError::new(MossTransportErrorCode::Io, true)
        }
    }
}

enum ReadLineFailure {
    TooLong,
    InvalidTerminator,
    Io,
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, ReadLineFailure> {
    let mut line = Vec::with_capacity(max_bytes.min(8 * 1024));
    loop {
        let available = reader.fill_buf().await.map_err(|_| ReadLineFailure::Io)?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(ReadLineFailure::InvalidTerminator)
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > max_bytes.saturating_add(1) {
            return Err(ReadLineFailure::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > max_bytes || line.is_empty() {
                return Err(ReadLineFailure::TooLong);
            }
            return Ok(Some(line));
        }
    }
}

struct PrivateStderrDiagnostics {
    bytes: Mutex<VecDeque<u8>>,
    capacity: usize,
}

impl PrivateStderrDiagnostics {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: Mutex::new(VecDeque::with_capacity(capacity.min(4 * 1024))),
            capacity,
        }
    }

    fn push(&self, incoming: &[u8]) {
        if self.capacity == 0 {
            return;
        }
        let mut bytes = self.bytes.lock().expect("private stderr buffer lock");
        for byte in incoming {
            if bytes.len() == self.capacity {
                bytes.pop_front();
            }
            bytes.push_back(*byte);
        }
    }
}

fn copy_environment_if_present(command: &mut Command, name: &str) {
    if let Some(value) = std::env::var_os(name) {
        command.env(name, value);
    }
}

fn safe_revision(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character))
}

fn validate_absolute_path(path: &Path) -> Result<(), MossProcessConfigError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(MossProcessConfigError::InvalidPath);
    }
    Ok(())
}

fn canonical_directory(path: &Path) -> Result<PathBuf, MossProcessConfigError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| MossProcessConfigError::InvalidPath)?;
    if !canonical.is_dir() {
        return Err(MossProcessConfigError::InvalidPath);
    }
    Ok(canonical)
}

fn canonical_directory_within(path: &Path, root: &Path) -> Result<PathBuf, MossProcessConfigError> {
    let canonical = canonical_directory(path)?;
    if !canonical.starts_with(root) {
        return Err(MossProcessConfigError::InvalidPath);
    }
    Ok(canonical)
}

fn create_canonical_directory_within(
    path: &Path,
    root: &Path,
) -> Result<PathBuf, MossProcessConfigError> {
    validate_descendant_allow_missing(path, root)?;
    std::fs::create_dir_all(path).map_err(|_| MossProcessConfigError::InvalidPath)?;
    canonical_directory_within(path, root)
}

fn canonical_file_within(path: &Path, root: &Path) -> Result<PathBuf, MossProcessConfigError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| MossProcessConfigError::InvalidPath)?;
    if !canonical.is_file() || !canonical.starts_with(root) {
        return Err(MossProcessConfigError::InvalidPath);
    }
    Ok(canonical)
}

fn validate_descendant_allow_missing(
    path: &Path,
    root: &Path,
) -> Result<(), MossProcessConfigError> {
    let _ = canonicalize_descendant_allow_missing(path, root)?;
    Ok(())
}

fn canonicalize_descendant_allow_missing(
    path: &Path,
    root: &Path,
) -> Result<PathBuf, MossProcessConfigError> {
    validate_absolute_path(path)?;
    if path.exists() {
        let canonical =
            std::fs::canonicalize(path).map_err(|_| MossProcessConfigError::InvalidPath)?;
        if !canonical.starts_with(root) {
            return Err(MossProcessConfigError::InvalidPath);
        }
        return Ok(canonical);
    }

    let mut ancestor = path.parent().ok_or(MossProcessConfigError::InvalidPath)?;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or(MossProcessConfigError::InvalidPath)?;
    }
    let canonical_ancestor =
        std::fs::canonicalize(ancestor).map_err(|_| MossProcessConfigError::InvalidPath)?;
    if !canonical_ancestor.starts_with(root) {
        return Err(MossProcessConfigError::InvalidPath);
    }
    let suffix = path
        .strip_prefix(ancestor)
        .map_err(|_| MossProcessConfigError::InvalidPath)?;
    Ok(canonical_ancestor.join(suffix))
}

#[cfg(windows)]
fn validate_windows_storage_path(path: &Path) -> Result<(), MossProcessConfigError> {
    use std::path::Prefix;
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)
                if letter.eq_ignore_ascii_case(&b'C') =>
            {
                Err(MossProcessConfigError::SystemDriveRejected)
            }
            Prefix::DeviceNS(_) | Prefix::Verbatim(_) => Err(MossProcessConfigError::InvalidPath),
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

#[cfg(not(windows))]
fn validate_windows_storage_path(_path: &Path) -> Result<(), MossProcessConfigError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::diarization::moss_protocol::MossProtocolLimits;
    use std::fs;
    use std::time::Instant;
    use tempfile::TempDir;

    struct FixturePaths {
        _portable: TempDir,
        portable_root: PathBuf,
        audio_root: PathBuf,
        audio_path: PathBuf,
        model_root: PathBuf,
        temp_root: PathBuf,
    }

    fn fixture_paths() -> FixturePaths {
        let portable = tempfile::tempdir().expect("create fixture portable root");
        let portable_root = portable.path().to_path_buf();
        let audio_root = portable_root.join("audio");
        let model_root = portable_root.join("models/moss-fixture");
        let temp_root = portable_root.join("temp");
        fs::create_dir_all(&audio_root).expect("create fixture audio root");
        let audio_path = audio_root.join("window.wav");
        fs::write(&audio_path, b"RIFF synthetic fixture").expect("write fixture audio");
        FixturePaths {
            _portable: portable,
            portable_root,
            audio_root,
            audio_path,
            model_root,
            temp_root,
        }
    }

    fn python_executable() -> PathBuf {
        if let Some(path) = std::env::var_os("MEETILY_TEST_PYTHON").map(PathBuf::from) {
            let canonical = fs::canonicalize(path).expect("canonicalize MEETILY_TEST_PYTHON");
            assert!(canonical.is_file(), "MEETILY_TEST_PYTHON must be a file");
            validate_windows_storage_path(&canonical)
                .expect("MEETILY_TEST_PYTHON must not be on C or a device namespace");
            return canonical;
        }
        for name in ["python3", "python"] {
            if let Ok(path) = which::which(name) {
                if validate_windows_storage_path(&path).is_err() {
                    continue;
                }
                return path;
            }
        }
        panic!("a non-C Python runtime is required for the ignored MOSS process integration tests")
    }

    fn local_config(paths: &FixturePaths) -> MossProcessTransportConfig {
        let executable_root = paths.portable_root.join("python-runtime");
        fs::create_dir_all(&executable_root).expect("create fake portable runtime");
        let python = executable_root.join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        fs::write(&python, b"synthetic executable marker").expect("write fake executable marker");
        let worker_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workers");
        let worker_script = worker_root.join("moss_worker.py");
        MossProcessTransportConfig::new(
            python,
            executable_root,
            worker_script,
            worker_root,
            &paths.portable_root,
            &paths.model_root,
            &paths.audio_root,
            &paths.temp_root,
            "fixture-model-v1",
        )
    }

    fn fixture_config(paths: &FixturePaths) -> MossProcessTransportConfig {
        let python = python_executable();
        let executable_root = python
            .parent()
            .expect("Python executable has a parent")
            .to_path_buf();
        let mut config = local_config(paths);
        config.python_executable = python;
        config.executable_root = executable_root;
        config.io_timeout = Duration::from_secs(2);
        config.max_jsonl_bytes = 64 * 1024;
        config.fixture_mode = true;
        config.allow_external_test_runtime = true;
        config
    }

    fn request(paths: &FixturePaths, job: &str, prompt: Option<&str>) -> MossWorkerRequest {
        MossWorkerRequest {
            schema: MOSS_WORKER_SCHEMA_VERSION,
            job: job.to_string(),
            session: "fixture-session".to_string(),
            window_start_frame: 0,
            window_end_frame: 48_000,
            audio_path: paths.audio_path.clone(),
            prompt: prompt.map(ToString::to_string),
            model_revision: "fixture-model-v1".to_string(),
        }
    }

    #[tokio::test]
    #[ignore = "requires an explicitly provisioned non-C Python runtime"]
    async fn fixture_process_handshakes_executes_and_shuts_down() {
        let paths = fixture_paths();
        let config = fixture_config(&paths);
        let factory = MossJsonlProcessFactory::new(config).expect("prepare fixture factory");
        let mut transport = factory.spawn().await.expect("spawn fixture worker");
        let handshake = transport.handshake().await.expect("fixture handshake");
        assert_eq!(handshake.schema, MOSS_WORKER_SCHEMA_VERSION);
        assert_eq!(handshake.worker_revision, "fixture-v1");
        assert_eq!(handshake.model_revision, "fixture-model-v1");
        let job = request(&paths, "fixture-job", None);
        let response = transport.execute(&job).await.expect("fixture result");
        response
            .validate_for_request(&job, MossProtocolLimits::default())
            .expect("valid fixture response");
        assert_eq!(response.segments.len(), 1);
        transport.shutdown().await.expect("fixture shutdown");
        assert!(!transport.is_alive());
    }

    #[tokio::test]
    #[ignore = "requires an explicitly provisioned non-C Python runtime"]
    async fn fixture_execution_can_exceed_control_timeout_but_obey_job_timeout() {
        let paths = fixture_paths();
        let config = fixture_config(&paths);
        let control_timeout = config.io_timeout;
        let factory = MossJsonlProcessFactory::new(config).expect("prepare fixture factory");
        let mut transport = factory.spawn().await.expect("spawn fixture worker");
        transport.handshake().await.expect("fixture handshake");
        let job = request(&paths, "fixture-delayed", Some("fixture:delay"));
        let started = Instant::now();
        let response = tokio::time::timeout(Duration::from_secs(4), transport.execute(&job))
            .await
            .expect("fixture result stayed within the job timeout")
            .expect("delayed fixture result");
        assert!(started.elapsed() > control_timeout);
        response
            .validate_for_request(&job, MossProtocolLimits::default())
            .expect("valid delayed fixture response");
        transport.shutdown().await.expect("fixture shutdown");
    }

    #[tokio::test]
    #[ignore = "requires an explicitly provisioned non-C Python runtime"]
    async fn fixture_process_supports_cancel_after_execute_future_is_dropped() {
        let paths = fixture_paths();
        let config = fixture_config(&paths);
        let factory = MossJsonlProcessFactory::new(config).expect("prepare fixture factory");
        let mut transport = factory.spawn().await.expect("spawn fixture worker");
        transport.handshake().await.expect("fixture handshake");
        let job = request(&paths, "fixture-cancel", Some("fixture:wait"));
        {
            let execution = transport.execute(&job);
            tokio::pin!(execution);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut execution)
                    .await
                    .is_err()
            );
        }
        transport
            .cancel(&job.job)
            .await
            .expect("cancel fixture job");
        transport.shutdown().await.expect("fixture shutdown");
    }

    #[tokio::test]
    #[ignore = "requires an explicitly provisioned non-C Python runtime"]
    async fn production_worker_reports_missing_model_without_synthetic_output() {
        let paths = fixture_paths();
        let mut config = fixture_config(&paths);
        config.fixture_mode = false;
        let factory = MossJsonlProcessFactory::new(config).expect("prepare production factory");
        let mut transport = factory.spawn().await.expect("spawn production worker");
        let error = transport
            .handshake()
            .await
            .expect_err("missing model must fail closed");
        assert_eq!(error.code, MossTransportErrorCode::ModelNotInstalled);
        assert!(!error.retryable);
        transport
            .shutdown()
            .await
            .expect("production worker shutdown");
    }

    #[tokio::test]
    #[ignore = "requires an explicitly provisioned non-C Python runtime"]
    async fn production_worker_reports_unavailable_adapter_without_running_inference() {
        let paths = fixture_paths();
        fs::create_dir_all(&paths.model_root).expect("create synthetic model directory");
        for name in [
            "added_tokens.json",
            "chat_template.jinja",
            "config.json",
            "configuration_moss_transcribe_diarize.py",
            "generation_config.json",
            "merges.txt",
            "model-00000-of-00001.safetensors",
            "model.safetensors.index.json",
            "modeling_moss_transcribe_diarize.py",
            "preprocessor_config.json",
            "processing_moss_transcribe_diarize.py",
            "processor_config.json",
            "special_tokens_map.json",
            "tokenizer.json",
            "tokenizer_config.json",
            "vocab.json",
            "model-revision.txt",
            "meetily-model-manifest.json",
        ] {
            fs::write(paths.model_root.join(name), b"synthetic invalid marker")
                .expect("write complete synthetic model marker set");
        }
        let mut config = fixture_config(&paths);
        config.fixture_mode = false;
        let factory = MossJsonlProcessFactory::new(config).expect("prepare production factory");
        let mut transport = factory.spawn().await.expect("spawn production worker");
        let error = transport
            .handshake()
            .await
            .expect_err("missing adapter must fail closed");
        assert_eq!(error.code, MossTransportErrorCode::RuntimeUnavailable);
        assert!(!error.retryable);
        transport
            .shutdown()
            .await
            .expect("production worker shutdown");
    }

    #[test]
    fn config_rejects_paths_outside_roots_and_redacts_debug() {
        let paths = fixture_paths();
        let mut config = local_config(&paths);
        let outside = tempfile::tempdir().expect("create outside root");
        config.audio_root = outside.path().to_path_buf();
        assert_eq!(
            MossJsonlProcessFactory::new(config.clone()).expect_err("outside audio root must fail"),
            MossProcessConfigError::InvalidPath
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains(&paths.portable_root.display().to_string()));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn production_config_rejects_a_python_runtime_outside_portable_root() {
        let paths = fixture_paths();
        let mut config = local_config(&paths);
        let outside = tempfile::tempdir().expect("create outside runtime root");
        let python = outside.path().join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        fs::write(&python, b"synthetic external executable")
            .expect("write external executable marker");
        config.python_executable = python;
        config.executable_root = outside.path().to_path_buf();
        assert_eq!(
            MossJsonlProcessFactory::new(config)
                .expect_err("external production runtime must fail"),
            MossProcessConfigError::InvalidPath
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_storage_policy_rejects_c_verbatim_and_device_paths() {
        assert_eq!(
            validate_windows_storage_path(Path::new(r"C:\Meetily")),
            Err(MossProcessConfigError::SystemDriveRejected)
        );
        assert_eq!(
            validate_windows_storage_path(Path::new(r"\\?\C:\Meetily")),
            Err(MossProcessConfigError::SystemDriveRejected)
        );
        assert_eq!(
            validate_windows_storage_path(Path::new(r"\\.\C:\Meetily")),
            Err(MossProcessConfigError::InvalidPath)
        );
        assert!(validate_windows_storage_path(Path::new(r"D:\Meetily")).is_ok());
        // `prepare` calls the same policy again after canonicalization. This
        // simulates a D-drive junction whose canonical target is on C.
        assert_eq!(
            validate_windows_storage_path(Path::new(r"C:\Windows")),
            Err(MossProcessConfigError::SystemDriveRejected)
        );
    }

    #[tokio::test]
    async fn bounded_reader_rejects_an_oversized_physical_line() {
        let payload = vec![b'x'; 33];
        let mut bytes = payload;
        bytes.push(b'\n');
        let mut reader = BufReader::new(bytes.as_slice());
        assert!(matches!(
            read_bounded_line(&mut reader, 32).await,
            Err(ReadLineFailure::TooLong)
        ));
    }
}
