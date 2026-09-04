//! Process-neutral lifecycle supervision for a future MOSS worker.
//!
//! The supervisor owns queueing, lifecycle, timeouts and failure isolation. It
//! deliberately does not know how a Python process is launched or how JSONL is
//! moved over pipes. A transport implementation must provide those details and
//! remain cancellation-safe when an in-flight future is dropped.

use super::moss_protocol::{
    MossProtocolLimits, MossWorkerRequest, MossWorkerResponse, MOSS_WORKER_SCHEMA_VERSION,
};
use async_trait::async_trait;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

const MOSS_WORKER_MAX_IN_FLIGHT: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MossWorkerState {
    Starting,
    Unavailable,
    Ready,
    Busy,
    Backoff,
    CircuitOpen,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MossWorkerErrorCode {
    InvalidConfiguration,
    RuntimeUnavailable,
    ModelNotInstalled,
    InvalidRequest,
    AudioUnavailable,
    AudioOutsideRoot,
    ModelRevisionMismatch,
    QueueFull,
    WorkerUnavailable,
    CircuitOpen,
    StartupTimeout,
    HandshakeRejected,
    WorkerCrashed,
    JobTimeout,
    JobCancelled,
    InvalidResponse,
    CancellationFailed,
    SupervisorStopping,
    SupervisorStopped,
    ShutdownTimeout,
    ShutdownFailed,
}

/// A caller-safe error. It never carries transport messages, file paths,
/// prompts, transcript text or provider response bodies.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MossWorkerError {
    pub code: MossWorkerErrorCode,
    pub retryable: bool,
}

impl MossWorkerError {
    pub const fn new(code: MossWorkerErrorCode, retryable: bool) -> Self {
        Self { code, retryable }
    }

    fn safe_message(self) -> &'static str {
        match self.code {
            MossWorkerErrorCode::InvalidConfiguration => "invalid worker supervisor configuration",
            MossWorkerErrorCode::RuntimeUnavailable => "async runtime is unavailable",
            MossWorkerErrorCode::ModelNotInstalled => "MOSS model is not installed",
            MossWorkerErrorCode::InvalidRequest => "worker request failed validation",
            MossWorkerErrorCode::AudioUnavailable => "audio snapshot is unavailable",
            MossWorkerErrorCode::AudioOutsideRoot => {
                "audio snapshot resolves outside the configured root"
            }
            MossWorkerErrorCode::ModelRevisionMismatch => {
                "request model revision does not match the worker"
            }
            MossWorkerErrorCode::QueueFull => "worker queue is full",
            MossWorkerErrorCode::WorkerUnavailable => "worker is temporarily unavailable",
            MossWorkerErrorCode::CircuitOpen => "worker circuit breaker is open",
            MossWorkerErrorCode::StartupTimeout => "worker startup timed out",
            MossWorkerErrorCode::HandshakeRejected => "worker handshake was rejected",
            MossWorkerErrorCode::WorkerCrashed => "worker exited unexpectedly",
            MossWorkerErrorCode::JobTimeout => "worker job timed out",
            MossWorkerErrorCode::JobCancelled => "worker job was cancelled",
            MossWorkerErrorCode::InvalidResponse => "worker response failed validation",
            MossWorkerErrorCode::CancellationFailed => "worker cancellation failed",
            MossWorkerErrorCode::SupervisorStopping => "worker supervisor is stopping",
            MossWorkerErrorCode::SupervisorStopped => "worker supervisor has stopped",
            MossWorkerErrorCode::ShutdownTimeout => "worker shutdown timed out",
            MossWorkerErrorCode::ShutdownFailed => "worker shutdown failed",
        }
    }
}

impl fmt::Debug for MossWorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossWorkerError")
            .field("code", &self.code)
            .field("retryable", &self.retryable)
            .finish()
    }
}

impl fmt::Display for MossWorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.safe_message())
    }
}

impl std::error::Error for MossWorkerError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MossTransportErrorCode {
    SpawnFailed,
    HandshakeFailed,
    RuntimeUnavailable,
    ModelNotInstalled,
    Crashed,
    Io,
    Protocol,
    CancelFailed,
    ShutdownFailed,
}

/// The transport may retain private diagnostics internally, but only this
/// redacted classification crosses into supervisor state or job results.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MossTransportError {
    pub code: MossTransportErrorCode,
    pub retryable: bool,
}

impl MossTransportError {
    pub const fn new(code: MossTransportErrorCode, retryable: bool) -> Self {
        Self { code, retryable }
    }
}

impl fmt::Debug for MossTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossTransportError")
            .field("code", &self.code)
            .field("retryable", &self.retryable)
            .finish()
    }
}

impl fmt::Display for MossTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MOSS transport failure: {:?}", self.code)
    }
}

impl std::error::Error for MossTransportError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MossWorkerHandshake {
    pub schema: u16,
    pub worker_revision: String,
    pub model_revision: String,
    pub max_in_flight: u8,
}

impl MossWorkerHandshake {
    fn validate(&self) -> Result<(), MossWorkerError> {
        let revisions_are_safe = [&self.worker_revision, &self.model_revision]
            .into_iter()
            .all(|value| {
                !value.is_empty()
                    && value.chars().count() <= 128
                    && value.chars().all(|character| {
                        character.is_ascii_alphanumeric() || "-_.:/".contains(character)
                    })
            });
        if self.schema != MOSS_WORKER_SCHEMA_VERSION
            || self.max_in_flight != MOSS_WORKER_MAX_IN_FLIGHT
            || !revisions_are_safe
        {
            return Err(MossWorkerError::new(
                MossWorkerErrorCode::HandshakeRejected,
                false,
            ));
        }
        Ok(())
    }
}

/// One live worker connection. `execute` is called for at most one job at a
/// time. `cancel` must leave the connection ready for another request or return
/// an error so the supervisor can recycle it.
#[async_trait]
pub trait MossWorkerTransport: Send {
    async fn handshake(&mut self) -> Result<MossWorkerHandshake, MossTransportError>;

    async fn execute(
        &mut self,
        request: &MossWorkerRequest,
    ) -> Result<MossWorkerResponse, MossTransportError>;

    async fn cancel(&mut self, job: &str) -> Result<(), MossTransportError>;

    async fn shutdown(&mut self) -> Result<(), MossTransportError>;

    /// Must be non-blocking. A process transport should update this from its
    /// child-exit watcher so idle crashes are detected without sending data.
    fn is_alive(&self) -> bool;
}

/// Creates one fresh connection. The future must be cancellation-safe because
/// shutdown may drop it before a process handle has been returned.
#[async_trait]
pub trait MossWorkerTransportFactory: Send + Sync {
    async fn spawn(&self) -> Result<Box<dyn MossWorkerTransport>, MossTransportError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MossWorkerBackoffPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub circuit_breaker_failures: u32,
    pub circuit_open_duration: Duration,
}

impl Default for MossWorkerBackoffPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(15),
            circuit_breaker_failures: 4,
            circuit_open_duration: Duration::from_secs(30),
        }
    }
}

#[derive(Clone)]
pub struct MossWorkerSupervisorConfig {
    pub allowed_audio_root: PathBuf,
    pub queue_capacity: usize,
    pub startup_timeout: Duration,
    pub job_timeout: Duration,
    pub control_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub health_check_interval: Duration,
    pub backoff: MossWorkerBackoffPolicy,
    pub protocol_limits: MossProtocolLimits,
}

impl MossWorkerSupervisorConfig {
    pub fn new(allowed_audio_root: impl Into<PathBuf>) -> Self {
        Self {
            allowed_audio_root: allowed_audio_root.into(),
            queue_capacity: 2,
            startup_timeout: Duration::from_secs(30),
            job_timeout: Duration::from_secs(180),
            control_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(10),
            health_check_interval: Duration::from_secs(1),
            backoff: MossWorkerBackoffPolicy::default(),
            protocol_limits: MossProtocolLimits::default(),
        }
    }

    fn prepare(self) -> Result<PreparedSupervisorConfig, MossWorkerError> {
        let durations_are_valid = [
            self.startup_timeout,
            self.job_timeout,
            self.control_timeout,
            self.shutdown_timeout,
            self.health_check_interval,
            self.backoff.initial_delay,
            self.backoff.max_delay,
            self.backoff.circuit_open_duration,
        ]
        .into_iter()
        .all(|duration| !duration.is_zero());
        if self.queue_capacity == 0
            || self.backoff.circuit_breaker_failures == 0
            || self.backoff.initial_delay > self.backoff.max_delay
            || !durations_are_valid
            || !self.allowed_audio_root.is_absolute()
        {
            return Err(MossWorkerError::new(
                MossWorkerErrorCode::InvalidConfiguration,
                false,
            ));
        }

        let canonical_audio_root = std::fs::canonicalize(&self.allowed_audio_root)
            .map_err(|_| MossWorkerError::new(MossWorkerErrorCode::InvalidConfiguration, false))?;
        if !canonical_audio_root.is_dir() {
            return Err(MossWorkerError::new(
                MossWorkerErrorCode::InvalidConfiguration,
                false,
            ));
        }
        Ok(PreparedSupervisorConfig {
            public: self,
            canonical_audio_root,
        })
    }
}

impl fmt::Debug for MossWorkerSupervisorConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossWorkerSupervisorConfig")
            .field("allowed_audio_root", &"<redacted>")
            .field("queue_capacity", &self.queue_capacity)
            .field("startup_timeout", &self.startup_timeout)
            .field("job_timeout", &self.job_timeout)
            .field("control_timeout", &self.control_timeout)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("health_check_interval", &self.health_check_interval)
            .field("backoff", &self.backoff)
            .field("protocol_limits", &self.protocol_limits)
            .finish()
    }
}

struct PreparedSupervisorConfig {
    public: MossWorkerSupervisorConfig,
    canonical_audio_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MossWorkerStatus {
    pub state: MossWorkerState,
    pub generation: u64,
    pub consecutive_failures: u32,
    pub active_job: Option<String>,
    pub last_error: Option<MossWorkerErrorCode>,
}

impl MossWorkerStatus {
    fn initial() -> Self {
        Self {
            state: MossWorkerState::Starting,
            generation: 0,
            consecutive_failures: 0,
            active_job: None,
            last_error: None,
        }
    }
}

#[derive(Clone)]
pub struct MossWorkerSupervisor {
    sender: mpsc::Sender<JobEnvelope>,
    shutdown: CancellationToken,
    status: watch::Receiver<MossWorkerStatus>,
    config: Arc<PreparedSupervisorConfig>,
}

impl MossWorkerSupervisor {
    pub fn start(
        config: MossWorkerSupervisorConfig,
        factory: Arc<dyn MossWorkerTransportFactory>,
    ) -> Result<Self, MossWorkerError> {
        tokio::runtime::Handle::try_current()
            .map_err(|_| MossWorkerError::new(MossWorkerErrorCode::RuntimeUnavailable, false))?;
        let config = Arc::new(config.prepare()?);
        let (sender, receiver) = mpsc::channel(config.public.queue_capacity);
        let shutdown = CancellationToken::new();
        let (status_sender, status) = watch::channel(MossWorkerStatus::initial());
        tokio::spawn(run_supervisor(
            Arc::clone(&config),
            factory,
            receiver,
            shutdown.clone(),
            status_sender,
        ));
        Ok(Self {
            sender,
            shutdown,
            status,
            config,
        })
    }

    pub fn try_submit(&self, request: MossWorkerRequest) -> Result<MossWorkerJob, MossWorkerError> {
        if self.shutdown.is_cancelled() {
            return Err(MossWorkerError::new(
                MossWorkerErrorCode::SupervisorStopping,
                false,
            ));
        }
        request
            .validate(
                &self.config.public.allowed_audio_root,
                self.config.public.protocol_limits,
            )
            .map_err(|_| MossWorkerError::new(MossWorkerErrorCode::InvalidRequest, false))?;

        match self.status.borrow().state {
            MossWorkerState::Unavailable => {
                let code = self
                    .status
                    .borrow()
                    .last_error
                    .unwrap_or(MossWorkerErrorCode::WorkerUnavailable);
                return Err(MossWorkerError::new(code, false));
            }
            MossWorkerState::Backoff => {
                return Err(MossWorkerError::new(
                    MossWorkerErrorCode::WorkerUnavailable,
                    true,
                ));
            }
            MossWorkerState::CircuitOpen => {
                return Err(MossWorkerError::new(MossWorkerErrorCode::CircuitOpen, true));
            }
            MossWorkerState::Stopping => {
                return Err(MossWorkerError::new(
                    MossWorkerErrorCode::SupervisorStopping,
                    false,
                ));
            }
            MossWorkerState::Stopped => {
                return Err(MossWorkerError::new(
                    MossWorkerErrorCode::SupervisorStopped,
                    false,
                ));
            }
            MossWorkerState::Starting | MossWorkerState::Ready | MossWorkerState::Busy => {}
        }

        let job = request.job.clone();
        let cancellation = CancellationToken::new();
        let (result_sender, result) = oneshot::channel();
        let envelope = JobEnvelope {
            request,
            cancellation: cancellation.clone(),
            result: result_sender,
        };
        self.sender
            .try_send(envelope)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    MossWorkerError::new(MossWorkerErrorCode::QueueFull, true)
                }
                mpsc::error::TrySendError::Closed(_) => {
                    MossWorkerError::new(MossWorkerErrorCode::SupervisorStopped, false)
                }
            })?;
        Ok(MossWorkerJob {
            job,
            cancellation,
            result,
        })
    }

    pub fn status(&self) -> MossWorkerStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe_status(&self) -> watch::Receiver<MossWorkerStatus> {
        self.status.clone()
    }

    pub fn request_shutdown(&self) {
        self.shutdown.cancel();
    }

    pub async fn shutdown_and_wait(&self, wait_timeout: Duration) -> Result<(), MossWorkerError> {
        self.request_shutdown();
        let mut status = self.subscribe_status();
        let stopped = tokio::time::timeout(wait_timeout, async {
            loop {
                if status.borrow().state == MossWorkerState::Stopped {
                    return status.borrow().last_error;
                }
                if status.changed().await.is_err() {
                    return Some(MossWorkerErrorCode::SupervisorStopped);
                }
            }
        })
        .await
        .map_err(|_| MossWorkerError::new(MossWorkerErrorCode::ShutdownTimeout, false))?;

        match stopped {
            Some(MossWorkerErrorCode::ShutdownTimeout) => Err(MossWorkerError::new(
                MossWorkerErrorCode::ShutdownTimeout,
                false,
            )),
            Some(MossWorkerErrorCode::ShutdownFailed) => Err(MossWorkerError::new(
                MossWorkerErrorCode::ShutdownFailed,
                false,
            )),
            _ => Ok(()),
        }
    }
}

pub struct MossWorkerJob {
    job: String,
    cancellation: CancellationToken,
    result: oneshot::Receiver<Result<MossWorkerResponse, MossWorkerError>>,
}

impl MossWorkerJob {
    pub fn id(&self) -> &str {
        &self.job
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn wait(self) -> Result<MossWorkerResponse, MossWorkerError> {
        self.result.await.unwrap_or_else(|_| {
            Err(MossWorkerError::new(
                MossWorkerErrorCode::SupervisorStopped,
                false,
            ))
        })
    }
}

impl fmt::Debug for MossWorkerJob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossWorkerJob")
            .field("job", &self.job)
            .finish_non_exhaustive()
    }
}

struct JobEnvelope {
    request: MossWorkerRequest,
    cancellation: CancellationToken,
    result: oneshot::Sender<Result<MossWorkerResponse, MossWorkerError>>,
}

enum StartAttempt {
    Ready(Box<dyn MossWorkerTransport>, MossWorkerHandshake),
    Shutdown,
}

struct JobExecution {
    result: Result<MossWorkerResponse, MossWorkerError>,
    restart_worker: bool,
    lifecycle_failure: Option<MossWorkerErrorCode>,
    shutdown_requested: bool,
}

async fn run_supervisor(
    config: Arc<PreparedSupervisorConfig>,
    factory: Arc<dyn MossWorkerTransportFactory>,
    mut receiver: mpsc::Receiver<JobEnvelope>,
    shutdown: CancellationToken,
    status: watch::Sender<MossWorkerStatus>,
) {
    let mut transport: Option<Box<dyn MossWorkerTransport>> = None;
    let mut handshake: Option<MossWorkerHandshake> = None;
    let mut generation = 0u64;
    let mut consecutive_failures = 0u32;
    let mut last_error = None;

    'supervisor: loop {
        if shutdown.is_cancelled() {
            break;
        }

        if transport.is_none() {
            publish_status(
                &status,
                MossWorkerState::Starting,
                generation,
                consecutive_failures,
                None,
                last_error,
            );
            match start_transport(&config, &factory, &shutdown).await {
                Ok(StartAttempt::Ready(new_transport, new_handshake)) => {
                    generation = generation.saturating_add(1);
                    transport = Some(new_transport);
                    handshake = Some(new_handshake);
                    publish_status(
                        &status,
                        MossWorkerState::Ready,
                        generation,
                        consecutive_failures,
                        None,
                        last_error,
                    );
                }
                Ok(StartAttempt::Shutdown) => break,
                Err(error) => {
                    last_error = Some(error.code);
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if matches!(
                        error.code,
                        MossWorkerErrorCode::RuntimeUnavailable
                            | MossWorkerErrorCode::ModelNotInstalled
                    ) {
                        publish_status(
                            &status,
                            MossWorkerState::Unavailable,
                            generation,
                            consecutive_failures,
                            None,
                            last_error,
                        );
                        drain_jobs(&mut receiver, error);
                        shutdown.cancelled().await;
                        break;
                    }
                    if wait_after_failure(
                        &config,
                        &mut receiver,
                        &shutdown,
                        &status,
                        generation,
                        consecutive_failures,
                        error.code,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
            }
        }

        let worker_is_alive = transport.as_ref().is_some_and(|current| current.is_alive());
        if !worker_is_alive {
            if let Some(mut dead) = transport.take() {
                let _ = stop_transport(&mut *dead, config.public.shutdown_timeout).await;
            }
            handshake = None;
            last_error = Some(MossWorkerErrorCode::WorkerCrashed);
            consecutive_failures = consecutive_failures.saturating_add(1);
            if wait_after_failure(
                &config,
                &mut receiver,
                &shutdown,
                &status,
                generation,
                consecutive_failures,
                MossWorkerErrorCode::WorkerCrashed,
            )
            .await
            {
                break;
            }
            continue;
        }

        let next = tokio::select! {
            biased;
            _ = shutdown.cancelled() => None,
            envelope = receiver.recv() => envelope,
            _ = tokio::time::sleep(config.public.health_check_interval) => {
                continue;
            }
        };
        let Some(envelope) = next else {
            break;
        };

        let active_job = envelope.request.job.clone();
        publish_status(
            &status,
            MossWorkerState::Busy,
            generation,
            consecutive_failures,
            Some(active_job),
            last_error,
        );
        let execution = execute_job(
            &mut **transport
                .as_mut()
                .expect("worker exists after readiness check"),
            handshake.as_ref().expect("handshake exists with worker"),
            &config,
            &shutdown,
            &envelope.request,
            &envelope.cancellation,
        )
        .await;
        let job_succeeded = execution.result.is_ok();
        let _ = envelope.result.send(execution.result);

        if execution.shutdown_requested {
            break 'supervisor;
        }
        if job_succeeded {
            consecutive_failures = 0;
            last_error = None;
        }
        if execution.restart_worker {
            if let Some(mut failed) = transport.take() {
                let _ = stop_transport(&mut *failed, config.public.shutdown_timeout).await;
            }
            handshake = None;
        }
        if let Some(code) = execution.lifecycle_failure {
            last_error = Some(code);
            consecutive_failures = consecutive_failures.saturating_add(1);
            if wait_after_failure(
                &config,
                &mut receiver,
                &shutdown,
                &status,
                generation,
                consecutive_failures,
                code,
            )
            .await
            {
                break;
            }
            continue;
        }
        if transport.is_some() {
            publish_status(
                &status,
                MossWorkerState::Ready,
                generation,
                consecutive_failures,
                None,
                last_error,
            );
        }
    }

    publish_status(
        &status,
        MossWorkerState::Stopping,
        generation,
        consecutive_failures,
        None,
        last_error,
    );
    receiver.close();
    drain_jobs(
        &mut receiver,
        MossWorkerError::new(MossWorkerErrorCode::SupervisorStopping, false),
    );
    let stop_error = if let Some(mut current) = transport.take() {
        stop_transport(&mut *current, config.public.shutdown_timeout)
            .await
            .err()
            .map(|error| error.code)
    } else {
        None
    };
    publish_status(
        &status,
        MossWorkerState::Stopped,
        generation,
        consecutive_failures,
        None,
        stop_error,
    );
}

async fn start_transport(
    config: &PreparedSupervisorConfig,
    factory: &Arc<dyn MossWorkerTransportFactory>,
    shutdown: &CancellationToken,
) -> Result<StartAttempt, MossWorkerError> {
    let spawned = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Ok(StartAttempt::Shutdown),
        result = tokio::time::timeout(config.public.startup_timeout, factory.spawn()) => result,
    };
    let mut transport = match spawned {
        Ok(Ok(transport)) => transport,
        Ok(Err(error)) => return Err(map_start_error(error)),
        Err(_) => {
            return Err(MossWorkerError::new(
                MossWorkerErrorCode::StartupTimeout,
                true,
            ));
        }
    };

    enum HandshakeRace {
        Finished(Result<MossWorkerHandshake, MossTransportError>),
        Timeout,
        Shutdown,
    }
    let handshake_race = {
        let handshake = transport.handshake();
        tokio::pin!(handshake);
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => HandshakeRace::Shutdown,
            _ = tokio::time::sleep(config.public.startup_timeout) => HandshakeRace::Timeout,
            result = &mut handshake => HandshakeRace::Finished(result),
        }
    };
    let worker_handshake = match handshake_race {
        HandshakeRace::Finished(Ok(handshake)) => handshake,
        HandshakeRace::Finished(Err(error)) => {
            let mapped = map_start_error(error);
            let _ = stop_transport(&mut *transport, config.public.shutdown_timeout).await;
            return Err(mapped);
        }
        HandshakeRace::Timeout => {
            let _ = stop_transport(&mut *transport, config.public.shutdown_timeout).await;
            return Err(MossWorkerError::new(
                MossWorkerErrorCode::StartupTimeout,
                true,
            ));
        }
        HandshakeRace::Shutdown => {
            let _ = stop_transport(&mut *transport, config.public.shutdown_timeout).await;
            return Ok(StartAttempt::Shutdown);
        }
    };
    if let Err(error) = worker_handshake.validate() {
        let _ = stop_transport(&mut *transport, config.public.shutdown_timeout).await;
        return Err(error);
    }
    if !transport.is_alive() {
        let _ = stop_transport(&mut *transport, config.public.shutdown_timeout).await;
        return Err(MossWorkerError::new(
            MossWorkerErrorCode::WorkerCrashed,
            true,
        ));
    }
    Ok(StartAttempt::Ready(transport, worker_handshake))
}

async fn execute_job(
    transport: &mut dyn MossWorkerTransport,
    handshake: &MossWorkerHandshake,
    config: &PreparedSupervisorConfig,
    shutdown: &CancellationToken,
    request: &MossWorkerRequest,
    cancellation: &CancellationToken,
) -> JobExecution {
    if cancellation.is_cancelled() {
        return JobExecution {
            result: Err(MossWorkerError::new(
                MossWorkerErrorCode::JobCancelled,
                false,
            )),
            restart_worker: false,
            lifecycle_failure: None,
            shutdown_requested: false,
        };
    }
    if request.model_revision != handshake.model_revision {
        return JobExecution {
            result: Err(MossWorkerError::new(
                MossWorkerErrorCode::ModelRevisionMismatch,
                false,
            )),
            restart_worker: false,
            lifecycle_failure: None,
            shutdown_requested: false,
        };
    }

    let dispatch_request = match canonicalize_dispatch_request(request, config).await {
        Ok(request) => request,
        Err(error) => {
            return JobExecution {
                result: Err(error),
                restart_worker: false,
                lifecycle_failure: None,
                shutdown_requested: false,
            };
        }
    };

    enum ExecutionRace {
        Finished(
            Result<Result<MossWorkerResponse, MossTransportError>, tokio::time::error::Elapsed>,
        ),
        Cancelled,
        Shutdown,
    }
    let race = {
        let execution = tokio::time::timeout(
            config.public.job_timeout,
            transport.execute(&dispatch_request),
        );
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => ExecutionRace::Shutdown,
            _ = cancellation.cancelled() => ExecutionRace::Cancelled,
            result = &mut execution => ExecutionRace::Finished(result),
        }
    };

    match race {
        ExecutionRace::Finished(Ok(Ok(response))) => {
            if response
                .validate_for_request(&dispatch_request, config.public.protocol_limits)
                .is_err()
            {
                JobExecution {
                    result: Err(MossWorkerError::new(
                        MossWorkerErrorCode::InvalidResponse,
                        false,
                    )),
                    restart_worker: true,
                    lifecycle_failure: Some(MossWorkerErrorCode::InvalidResponse),
                    shutdown_requested: false,
                }
            } else {
                JobExecution {
                    result: Ok(response),
                    restart_worker: false,
                    lifecycle_failure: None,
                    shutdown_requested: false,
                }
            }
        }
        ExecutionRace::Finished(Ok(Err(error))) => {
            let mapped = map_runtime_error(error);
            JobExecution {
                result: Err(mapped),
                restart_worker: true,
                lifecycle_failure: Some(mapped.code),
                shutdown_requested: false,
            }
        }
        ExecutionRace::Finished(Err(_)) => {
            let _ = cancel_job(
                transport,
                &dispatch_request.job,
                config.public.control_timeout,
            )
            .await;
            JobExecution {
                result: Err(MossWorkerError::new(MossWorkerErrorCode::JobTimeout, true)),
                restart_worker: true,
                lifecycle_failure: Some(MossWorkerErrorCode::JobTimeout),
                shutdown_requested: false,
            }
        }
        ExecutionRace::Cancelled => {
            let cancelled = cancel_job(
                transport,
                &dispatch_request.job,
                config.public.control_timeout,
            )
            .await;
            JobExecution {
                result: Err(MossWorkerError::new(
                    MossWorkerErrorCode::JobCancelled,
                    false,
                )),
                restart_worker: cancelled.is_err(),
                lifecycle_failure: cancelled
                    .err()
                    .map(|_| MossWorkerErrorCode::CancellationFailed),
                shutdown_requested: false,
            }
        }
        ExecutionRace::Shutdown => {
            let _ = cancel_job(
                transport,
                &dispatch_request.job,
                config.public.control_timeout,
            )
            .await;
            JobExecution {
                result: Err(MossWorkerError::new(
                    MossWorkerErrorCode::SupervisorStopping,
                    false,
                )),
                restart_worker: false,
                lifecycle_failure: None,
                shutdown_requested: true,
            }
        }
    }
}

async fn canonicalize_dispatch_request(
    request: &MossWorkerRequest,
    config: &PreparedSupervisorConfig,
) -> Result<MossWorkerRequest, MossWorkerError> {
    request
        .validate(
            &config.public.allowed_audio_root,
            config.public.protocol_limits,
        )
        .map_err(|_| MossWorkerError::new(MossWorkerErrorCode::InvalidRequest, false))?;
    let canonical_path = tokio::fs::canonicalize(&request.audio_path)
        .await
        .map_err(|_| MossWorkerError::new(MossWorkerErrorCode::AudioUnavailable, true))?;
    ensure_canonical_containment(&canonical_path, &config.canonical_audio_root)?;
    let metadata = tokio::fs::metadata(&canonical_path)
        .await
        .map_err(|_| MossWorkerError::new(MossWorkerErrorCode::AudioUnavailable, true))?;
    if !metadata.is_file() {
        return Err(MossWorkerError::new(
            MossWorkerErrorCode::AudioUnavailable,
            true,
        ));
    }
    let mut canonical_request = request.clone();
    canonical_request.audio_path = canonical_path;
    canonical_request
        .validate(&config.canonical_audio_root, config.public.protocol_limits)
        .map_err(|_| MossWorkerError::new(MossWorkerErrorCode::InvalidRequest, false))?;
    Ok(canonical_request)
}

fn ensure_canonical_containment(
    canonical_path: &Path,
    canonical_root: &Path,
) -> Result<(), MossWorkerError> {
    if !canonical_path.starts_with(canonical_root) {
        return Err(MossWorkerError::new(
            MossWorkerErrorCode::AudioOutsideRoot,
            false,
        ));
    }
    Ok(())
}

async fn cancel_job(
    transport: &mut dyn MossWorkerTransport,
    job: &str,
    timeout: Duration,
) -> Result<(), MossWorkerError> {
    match tokio::time::timeout(timeout, transport.cancel(job)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(MossWorkerError::new(
            MossWorkerErrorCode::CancellationFailed,
            true,
        )),
    }
}

async fn stop_transport(
    transport: &mut dyn MossWorkerTransport,
    timeout: Duration,
) -> Result<(), MossWorkerError> {
    match tokio::time::timeout(timeout, transport.shutdown()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(MossWorkerError::new(
            MossWorkerErrorCode::ShutdownFailed,
            false,
        )),
        Err(_) => Err(MossWorkerError::new(
            MossWorkerErrorCode::ShutdownTimeout,
            false,
        )),
    }
}

fn map_start_error(error: MossTransportError) -> MossWorkerError {
    match error.code {
        MossTransportErrorCode::RuntimeUnavailable => {
            MossWorkerError::new(MossWorkerErrorCode::RuntimeUnavailable, false)
        }
        MossTransportErrorCode::ModelNotInstalled => {
            MossWorkerError::new(MossWorkerErrorCode::ModelNotInstalled, false)
        }
        MossTransportErrorCode::Crashed => {
            MossWorkerError::new(MossWorkerErrorCode::WorkerCrashed, true)
        }
        MossTransportErrorCode::HandshakeFailed | MossTransportErrorCode::Protocol => {
            MossWorkerError::new(MossWorkerErrorCode::HandshakeRejected, error.retryable)
        }
        _ => MossWorkerError::new(MossWorkerErrorCode::WorkerUnavailable, error.retryable),
    }
}

fn map_runtime_error(error: MossTransportError) -> MossWorkerError {
    match error.code {
        MossTransportErrorCode::RuntimeUnavailable => {
            MossWorkerError::new(MossWorkerErrorCode::RuntimeUnavailable, false)
        }
        MossTransportErrorCode::ModelNotInstalled => {
            MossWorkerError::new(MossWorkerErrorCode::ModelNotInstalled, false)
        }
        MossTransportErrorCode::Crashed => {
            MossWorkerError::new(MossWorkerErrorCode::WorkerCrashed, true)
        }
        MossTransportErrorCode::Protocol => {
            MossWorkerError::new(MossWorkerErrorCode::InvalidResponse, false)
        }
        MossTransportErrorCode::CancelFailed => {
            MossWorkerError::new(MossWorkerErrorCode::CancellationFailed, true)
        }
        _ => MossWorkerError::new(MossWorkerErrorCode::WorkerUnavailable, error.retryable),
    }
}

async fn wait_after_failure(
    config: &PreparedSupervisorConfig,
    receiver: &mut mpsc::Receiver<JobEnvelope>,
    shutdown: &CancellationToken,
    status: &watch::Sender<MossWorkerStatus>,
    generation: u64,
    consecutive_failures: u32,
    error: MossWorkerErrorCode,
) -> bool {
    let circuit_open = consecutive_failures >= config.public.backoff.circuit_breaker_failures;
    let (state, delay) = if circuit_open {
        (
            MossWorkerState::CircuitOpen,
            config.public.backoff.circuit_open_duration,
        )
    } else {
        (
            MossWorkerState::Backoff,
            exponential_backoff(config.public.backoff, consecutive_failures),
        )
    };
    publish_status(
        status,
        state,
        generation,
        consecutive_failures,
        None,
        Some(error),
    );
    if circuit_open {
        drain_jobs(
            receiver,
            MossWorkerError::new(MossWorkerErrorCode::CircuitOpen, true),
        );
    }
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

fn exponential_backoff(policy: MossWorkerBackoffPolicy, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(31);
    let multiplier = 1u128 << exponent;
    let initial_millis = policy.initial_delay.as_millis();
    let max_millis = policy.max_delay.as_millis();
    let delay_millis = initial_millis.saturating_mul(multiplier).min(max_millis);
    Duration::from_millis(delay_millis.min(u64::MAX as u128) as u64)
}

fn drain_jobs(receiver: &mut mpsc::Receiver<JobEnvelope>, error: MossWorkerError) {
    while let Ok(envelope) = receiver.try_recv() {
        let _ = envelope.result.send(Err(error));
    }
}

fn publish_status(
    status: &watch::Sender<MossWorkerStatus>,
    state: MossWorkerState,
    generation: u64,
    consecutive_failures: u32,
    active_job: Option<String>,
    last_error: Option<MossWorkerErrorCode>,
) {
    status.send_replace(MossWorkerStatus {
        state,
        generation,
        consecutive_failures,
        active_job,
        last_error,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[derive(Clone)]
    enum HandshakeBehavior {
        Ready,
        Invalid,
        Unavailable(MossTransportErrorCode),
        Pending,
    }

    #[derive(Clone)]
    enum ExecuteBehavior {
        Success(Duration),
        Pending,
        Crash,
    }

    struct FakePlan {
        handshake: HandshakeBehavior,
        executions: VecDeque<ExecuteBehavior>,
        alive: Arc<AtomicBool>,
    }

    impl FakePlan {
        fn ready(executions: impl IntoIterator<Item = ExecuteBehavior>) -> Self {
            Self {
                handshake: HandshakeBehavior::Ready,
                executions: executions.into_iter().collect(),
                alive: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    #[derive(Default)]
    struct FakeMetrics {
        spawns: AtomicUsize,
        active_jobs: AtomicUsize,
        max_active_jobs: AtomicUsize,
        cancels: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    struct FakeFactory {
        plans: Mutex<VecDeque<FakePlan>>,
        metrics: Arc<FakeMetrics>,
    }

    impl FakeFactory {
        fn new(plans: impl IntoIterator<Item = FakePlan>) -> Self {
            Self {
                plans: Mutex::new(plans.into_iter().collect()),
                metrics: Arc::new(FakeMetrics::default()),
            }
        }
    }

    #[async_trait]
    impl MossWorkerTransportFactory for FakeFactory {
        async fn spawn(&self) -> Result<Box<dyn MossWorkerTransport>, MossTransportError> {
            self.metrics.spawns.fetch_add(1, Ordering::SeqCst);
            let plan = self
                .plans
                .lock()
                .expect("fake plans lock")
                .pop_front()
                .ok_or_else(|| {
                    MossTransportError::new(MossTransportErrorCode::SpawnFailed, true)
                })?;
            Ok(Box::new(FakeTransport {
                plan,
                metrics: Arc::clone(&self.metrics),
            }))
        }
    }

    struct FakeTransport {
        plan: FakePlan,
        metrics: Arc<FakeMetrics>,
    }

    struct ActiveJobGuard(Arc<FakeMetrics>);

    impl Drop for ActiveJobGuard {
        fn drop(&mut self) {
            self.0.active_jobs.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl MossWorkerTransport for FakeTransport {
        async fn handshake(&mut self) -> Result<MossWorkerHandshake, MossTransportError> {
            match self.plan.handshake {
                HandshakeBehavior::Ready => Ok(MossWorkerHandshake {
                    schema: MOSS_WORKER_SCHEMA_VERSION,
                    worker_revision: "worker-test".to_string(),
                    model_revision: "model-test".to_string(),
                    max_in_flight: MOSS_WORKER_MAX_IN_FLIGHT,
                }),
                HandshakeBehavior::Invalid => Ok(MossWorkerHandshake {
                    schema: MOSS_WORKER_SCHEMA_VERSION.saturating_add(1),
                    worker_revision: "worker-test".to_string(),
                    model_revision: "model-test".to_string(),
                    max_in_flight: MOSS_WORKER_MAX_IN_FLIGHT.saturating_add(1),
                }),
                HandshakeBehavior::Unavailable(code) => Err(MossTransportError::new(code, false)),
                HandshakeBehavior::Pending => std::future::pending().await,
            }
        }

        async fn execute(
            &mut self,
            request: &MossWorkerRequest,
        ) -> Result<MossWorkerResponse, MossTransportError> {
            let active = self.metrics.active_jobs.fetch_add(1, Ordering::SeqCst) + 1;
            self.metrics
                .max_active_jobs
                .fetch_max(active, Ordering::SeqCst);
            let _guard = ActiveJobGuard(Arc::clone(&self.metrics));
            match self
                .plan
                .executions
                .pop_front()
                .unwrap_or(ExecuteBehavior::Success(Duration::ZERO))
            {
                ExecuteBehavior::Success(delay) => {
                    tokio::time::sleep(delay).await;
                    Ok(MossWorkerResponse {
                        schema: request.schema,
                        job: request.job.clone(),
                        session: request.session.clone(),
                        window_start_frame: request.window_start_frame,
                        window_end_frame: request.window_end_frame,
                        segments: Vec::new(),
                    })
                }
                ExecuteBehavior::Pending => std::future::pending().await,
                ExecuteBehavior::Crash => {
                    self.plan.alive.store(false, Ordering::SeqCst);
                    Err(MossTransportError::new(
                        MossTransportErrorCode::Crashed,
                        true,
                    ))
                }
            }
        }

        async fn cancel(&mut self, _job: &str) -> Result<(), MossTransportError> {
            self.metrics.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), MossTransportError> {
            self.metrics.shutdowns.fetch_add(1, Ordering::SeqCst);
            self.plan.alive.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn is_alive(&self) -> bool {
            self.plan.alive.load(Ordering::SeqCst)
        }
    }

    struct TestAudio {
        _directory: TempDir,
        root: PathBuf,
        file: PathBuf,
    }

    fn test_audio() -> TestAudio {
        let directory = tempfile::tempdir().expect("create test audio root");
        let root = directory.path().to_path_buf();
        let file = root.join("window.wav");
        fs::write(&file, b"RIFF-test").expect("write synthetic snapshot");
        TestAudio {
            _directory: directory,
            root,
            file,
        }
    }

    fn request(audio: &TestAudio, job: &str) -> MossWorkerRequest {
        MossWorkerRequest {
            schema: MOSS_WORKER_SCHEMA_VERSION,
            job: job.to_string(),
            session: "session-test".to_string(),
            window_start_frame: 0,
            window_end_frame: 48_000,
            audio_path: audio.file.clone(),
            prompt: Some("synthetic prompt".to_string()),
            model_revision: "model-test".to_string(),
        }
    }

    fn test_config(root: &Path) -> MossWorkerSupervisorConfig {
        let mut config = MossWorkerSupervisorConfig::new(root);
        config.queue_capacity = 2;
        config.startup_timeout = Duration::from_millis(100);
        config.job_timeout = Duration::from_millis(40);
        config.control_timeout = Duration::from_millis(20);
        config.shutdown_timeout = Duration::from_millis(20);
        config.health_check_interval = Duration::from_millis(5);
        config.backoff = MossWorkerBackoffPolicy {
            initial_delay: Duration::from_millis(2),
            max_delay: Duration::from_millis(5),
            circuit_breaker_failures: 2,
            circuit_open_duration: Duration::from_millis(100),
        };
        config
    }

    async fn wait_for_state(supervisor: &MossWorkerSupervisor, expected: MossWorkerState) {
        let mut status = supervisor.subscribe_status();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if status.borrow().state == expected {
                    return;
                }
                status
                    .changed()
                    .await
                    .expect("supervisor status remains open");
            }
        })
        .await
        .expect("expected supervisor state");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_and_jobs_are_strictly_single_task() {
        let audio = test_audio();
        let factory = Arc::new(FakeFactory::new([FakePlan::ready([
            ExecuteBehavior::Success(Duration::from_millis(10)),
            ExecuteBehavior::Success(Duration::from_millis(10)),
        ])]));
        let metrics = Arc::clone(&factory.metrics);
        let supervisor = MossWorkerSupervisor::start(test_config(&audio.root), factory)
            .expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::Ready).await;

        let first = supervisor
            .try_submit(request(&audio, "job-one"))
            .expect("queue first");
        let second = supervisor
            .try_submit(request(&audio, "job-two"))
            .expect("queue second");
        let (first_result, second_result) = tokio::join!(first.wait(), second.wait());
        assert!(first_result.is_ok());
        assert!(second_result.is_ok());
        assert_eq!(metrics.max_active_jobs.load(Ordering::SeqCst), 1);
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop supervisor");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_is_bounded_while_handshake_is_pending() {
        let audio = test_audio();
        let mut config = test_config(&audio.root);
        config.queue_capacity = 1;
        config.startup_timeout = Duration::from_secs(1);
        let factory = Arc::new(FakeFactory::new([FakePlan {
            handshake: HandshakeBehavior::Pending,
            executions: VecDeque::new(),
            alive: Arc::new(AtomicBool::new(true)),
        }]));
        let supervisor = MossWorkerSupervisor::start(config, factory).expect("start supervisor");
        let queued = supervisor
            .try_submit(request(&audio, "job-queued"))
            .expect("first job fits");
        let error = supervisor
            .try_submit(request(&audio, "job-overflow"))
            .expect_err("second job exceeds capacity");
        assert_eq!(error.code, MossWorkerErrorCode::QueueFull);
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop during handshake");
        assert_eq!(
            queued.wait().await.expect_err("queued job is stopped").code,
            MossWorkerErrorCode::SupervisorStopping
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_handshake_is_rejected_before_any_job_runs() {
        let audio = test_audio();
        let mut config = test_config(&audio.root);
        config.backoff.circuit_breaker_failures = 1;
        let factory = Arc::new(FakeFactory::new([FakePlan {
            handshake: HandshakeBehavior::Invalid,
            executions: VecDeque::new(),
            alive: Arc::new(AtomicBool::new(true)),
        }]));
        let metrics = Arc::clone(&factory.metrics);
        let supervisor = MossWorkerSupervisor::start(config, factory).expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::CircuitOpen).await;
        assert_eq!(
            supervisor.status().last_error,
            Some(MossWorkerErrorCode::HandshakeRejected)
        );
        assert_eq!(metrics.active_jobs.load(Ordering::SeqCst), 0);
        assert_eq!(
            supervisor
                .try_submit(request(&audio, "job-before-handshake"))
                .expect_err("invalid handshake rejects jobs")
                .code,
            MossWorkerErrorCode::CircuitOpen
        );
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop supervisor");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_model_stays_unavailable_without_a_respawn_loop() {
        let audio = test_audio();
        let factory = Arc::new(FakeFactory::new([FakePlan {
            handshake: HandshakeBehavior::Unavailable(MossTransportErrorCode::ModelNotInstalled),
            executions: VecDeque::new(),
            alive: Arc::new(AtomicBool::new(true)),
        }]));
        let metrics = Arc::clone(&factory.metrics);
        let supervisor = MossWorkerSupervisor::start(test_config(&audio.root), factory)
            .expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::Unavailable).await;
        assert_eq!(
            supervisor.status().last_error,
            Some(MossWorkerErrorCode::ModelNotInstalled)
        );
        assert_eq!(metrics.spawns.load(Ordering::SeqCst), 1);
        assert_eq!(
            supervisor
                .try_submit(request(&audio, "job-without-model"))
                .expect_err("unavailable worker rejects jobs"),
            MossWorkerError::new(MossWorkerErrorCode::ModelNotInstalled, false)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(metrics.spawns.load(Ordering::SeqCst), 1);
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop unavailable supervisor");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_job_is_cancelled_and_worker_is_restarted() {
        let audio = test_audio();
        let factory = Arc::new(FakeFactory::new([
            FakePlan::ready([ExecuteBehavior::Pending]),
            FakePlan::ready([ExecuteBehavior::Success(Duration::ZERO)]),
        ]));
        let metrics = Arc::clone(&factory.metrics);
        let supervisor = MossWorkerSupervisor::start(test_config(&audio.root), factory)
            .expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        let timed_out = supervisor
            .try_submit(request(&audio, "job-timeout"))
            .expect("queue timeout job")
            .wait()
            .await
            .expect_err("job must time out");
        assert_eq!(timed_out.code, MossWorkerErrorCode::JobTimeout);
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        assert!(supervisor
            .try_submit(request(&audio, "job-after-timeout"))
            .expect("queue recovery job")
            .wait()
            .await
            .is_ok());
        assert_eq!(supervisor.status().consecutive_failures, 0);
        assert_eq!(supervisor.status().last_error, None);
        assert!(metrics.spawns.load(Ordering::SeqCst) >= 2);
        assert!(metrics.cancels.load(Ordering::SeqCst) >= 1);
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop supervisor");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_job_can_be_cancelled_without_leaking_payloads() {
        let audio = test_audio();
        let factory = Arc::new(FakeFactory::new([FakePlan::ready([
            ExecuteBehavior::Pending,
        ])]));
        let metrics = Arc::clone(&factory.metrics);
        let supervisor = MossWorkerSupervisor::start(test_config(&audio.root), factory)
            .expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        let job = supervisor
            .try_submit(request(&audio, "job-cancel"))
            .expect("queue job");
        wait_for_state(&supervisor, MossWorkerState::Busy).await;
        job.cancel();
        assert_eq!(
            job.wait().await.expect_err("cancelled job").code,
            MossWorkerErrorCode::JobCancelled
        );
        assert_eq!(metrics.cancels.load(Ordering::SeqCst), 1);
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop supervisor");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_crashes_open_the_circuit_breaker() {
        let audio = test_audio();
        let factory = Arc::new(FakeFactory::new([
            FakePlan::ready([ExecuteBehavior::Crash]),
            FakePlan::ready([ExecuteBehavior::Crash]),
        ]));
        let supervisor = MossWorkerSupervisor::start(test_config(&audio.root), factory)
            .expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        assert_eq!(
            supervisor
                .try_submit(request(&audio, "job-crash-one"))
                .expect("queue first crash")
                .wait()
                .await
                .expect_err("first worker crashes")
                .code,
            MossWorkerErrorCode::WorkerCrashed
        );
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        assert_eq!(
            supervisor
                .try_submit(request(&audio, "job-crash-two"))
                .expect("queue second crash")
                .wait()
                .await
                .expect_err("second worker crashes")
                .code,
            MossWorkerErrorCode::WorkerCrashed
        );
        wait_for_state(&supervisor, MossWorkerState::CircuitOpen).await;
        assert_eq!(
            supervisor
                .try_submit(request(&audio, "job-rejected"))
                .expect_err("open circuit rejects work")
                .code,
            MossWorkerErrorCode::CircuitOpen
        );
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop supervisor");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_process_exit_is_detected_and_restarted() {
        let audio = test_audio();
        let first_alive = Arc::new(AtomicBool::new(true));
        let first = FakePlan {
            handshake: HandshakeBehavior::Ready,
            executions: VecDeque::new(),
            alive: Arc::clone(&first_alive),
        };
        let factory = Arc::new(FakeFactory::new([
            first,
            FakePlan::ready([ExecuteBehavior::Success(Duration::ZERO)]),
        ]));
        let metrics = Arc::clone(&factory.metrics);
        let supervisor = MossWorkerSupervisor::start(test_config(&audio.root), factory)
            .expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        first_alive.store(false, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(1), async {
            while metrics.spawns.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("worker is restarted after idle exit");
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop supervisor");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graceful_shutdown_cancels_active_and_queued_jobs() {
        let audio = test_audio();
        let factory = Arc::new(FakeFactory::new([FakePlan::ready([
            ExecuteBehavior::Pending,
        ])]));
        let metrics = Arc::clone(&factory.metrics);
        let supervisor = MossWorkerSupervisor::start(test_config(&audio.root), factory)
            .expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        let active = supervisor
            .try_submit(request(&audio, "job-active"))
            .expect("queue active");
        wait_for_state(&supervisor, MossWorkerState::Busy).await;
        let queued = supervisor
            .try_submit(request(&audio, "job-waiting"))
            .expect("queue waiting");
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("graceful stop");
        assert_eq!(
            active.wait().await.expect_err("active job stopped").code,
            MossWorkerErrorCode::SupervisorStopping
        );
        assert_eq!(
            queued.wait().await.expect_err("queued job stopped").code,
            MossWorkerErrorCode::SupervisorStopping
        );
        assert!(metrics.cancels.load(Ordering::SeqCst) >= 1);
        assert!(metrics.shutdowns.load(Ordering::SeqCst) >= 1);
        assert_eq!(supervisor.status().state, MossWorkerState::Stopped);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_snapshot_is_rejected_before_transport_execution() {
        let audio = test_audio();
        let factory = Arc::new(FakeFactory::new([FakePlan::ready([
            ExecuteBehavior::Success(Duration::ZERO),
        ])]));
        let metrics = Arc::clone(&factory.metrics);
        let supervisor = MossWorkerSupervisor::start(test_config(&audio.root), factory)
            .expect("start supervisor");
        wait_for_state(&supervisor, MossWorkerState::Ready).await;
        fs::remove_file(&audio.file).expect("remove synthetic snapshot");
        assert_eq!(
            supervisor
                .try_submit(request(&audio, "job-missing-audio"))
                .expect("queue request before dispatch check")
                .wait()
                .await
                .expect_err("missing snapshot is rejected")
                .code,
            MossWorkerErrorCode::AudioUnavailable
        );
        assert_eq!(metrics.active_jobs.load(Ordering::SeqCst), 0);
        supervisor
            .shutdown_and_wait(Duration::from_secs(1))
            .await
            .expect("stop supervisor");
    }

    #[test]
    fn canonical_containment_and_debug_output_are_safe() {
        let inside = test_audio();
        let outside = test_audio();
        let canonical_root = fs::canonicalize(&inside.root).expect("canonical inside root");
        let canonical_outside = fs::canonicalize(&outside.file).expect("canonical outside file");
        assert_eq!(
            ensure_canonical_containment(&canonical_outside, &canonical_root)
                .expect_err("outside canonical path is rejected")
                .code,
            MossWorkerErrorCode::AudioOutsideRoot
        );

        let config = MossWorkerSupervisorConfig::new(&inside.root);
        let debug = format!("{config:?}");
        assert!(!debug.contains(&inside.root.display().to_string()));
        assert!(debug.contains("<redacted>"));
        let error = MossWorkerError::new(MossWorkerErrorCode::InvalidResponse, false);
        assert_eq!(
            format!("{error:?}"),
            "MossWorkerError { code: InvalidResponse, retryable: false }"
        );
    }
}
