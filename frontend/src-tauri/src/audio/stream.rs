use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Stream, SupportedStreamConfig};
use log::{error, info, warn};
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread::JoinHandle;
use tokio::sync::mpsc;

use super::capture::{get_current_backend, AudioCaptureBackend};
use super::devices::{get_device_and_config, AudioDevice};
use super::pipeline::AudioCapture;
use super::recording_state::{DeviceType, RecordingState};

/// One selected capture route that could not be opened at session startup.
///
/// A dual-source recording may continue on the healthy route, but callers
/// still need a structured warning instead of relying on log text.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStreamStartupFailure {
    pub device_type: DeviceType,
    pub device_label: String,
    pub detail: String,
}

#[cfg(target_os = "macos")]
use super::capture::CoreAudioCapture;

enum StreamOwnerCommand {
    Stop(std_mpsc::SyncSender<Result<()>>),
}

/// Send-safe control plane for a resource that never leaves its owner thread.
///
/// CPAL deliberately does not promise that `Stream` can move between threads on
/// every backend. Only this command sender and join handle cross Tokio worker
/// boundaries; the stream itself is created, played, paused and dropped inside
/// the spawned OS thread.
struct ThreadOwnedStream {
    command_sender: Option<std_mpsc::Sender<StreamOwnerCommand>>,
    owner_thread: Option<JoinHandle<()>>,
}

impl ThreadOwnedStream {
    async fn spawn<T, Create, Stop>(
        thread_name: String,
        create_and_start: Create,
        stop_and_drop: Stop,
    ) -> Result<Self>
    where
        T: 'static,
        Create: FnOnce() -> Result<T> + Send + 'static,
        Stop: FnOnce(T) -> Result<()> + Send + 'static,
    {
        let (command_sender, command_receiver) = std_mpsc::channel();
        let (startup_sender, startup_receiver) = tokio::sync::oneshot::channel();

        let owner_thread = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || match create_and_start() {
                Ok(resource) => {
                    if startup_sender.send(Ok(())).is_err() {
                        let _ = stop_and_drop(resource);
                        return;
                    }

                    match command_receiver.recv() {
                        Ok(StreamOwnerCommand::Stop(completion_sender)) => {
                            let result = stop_and_drop(resource);
                            let _ = completion_sender.send(result);
                        }
                        Err(_) => {
                            // The controller disappeared without an explicit
                            // stop. Resource cleanup must still run on this
                            // owner thread.
                            let _ = stop_and_drop(resource);
                        }
                    }
                }
                Err(error) => {
                    let _ = startup_sender.send(Err(error));
                }
            })
            .map_err(|error| anyhow::anyhow!("Failed to spawn audio owner thread: {error}"))?;

        match startup_receiver.await {
            Ok(Ok(())) => Ok(Self {
                command_sender: Some(command_sender),
                owner_thread: Some(owner_thread),
            }),
            Ok(Err(error)) => {
                let _ = owner_thread.join();
                Err(error)
            }
            Err(_) => {
                let join_detail = owner_thread
                    .join()
                    .err()
                    .map(|_| "; owner thread panicked")
                    .unwrap_or_default();
                Err(anyhow::anyhow!(
                    "Audio owner thread ended before startup completed{join_detail}"
                ))
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        let mut errors = Vec::new();

        if let Some(command_sender) = self.command_sender.take() {
            let (completion_sender, completion_receiver) = std_mpsc::sync_channel(1);
            if command_sender
                .send(StreamOwnerCommand::Stop(completion_sender))
                .is_err()
            {
                errors.push("audio owner thread stopped before receiving the stop command".into());
            } else {
                match completion_receiver.recv() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => errors.push(error.to_string()),
                    Err(_) => errors
                        .push("audio owner thread ended without confirming stream cleanup".into()),
                }
            }
        }

        if let Some(owner_thread) = self.owner_thread.take() {
            if owner_thread.join().is_err() {
                errors.push("audio owner thread panicked during stream cleanup".into());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(errors.join("; ")))
        }
    }
}

impl Drop for ThreadOwnedStream {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            warn!("Failed to stop thread-owned audio stream: {error}");
        }
    }
}

/// Stream backend implementation. No backend resource relies on a manual
/// `Send` implementation.
enum StreamBackend {
    /// CPAL stream controlled through its dedicated owner thread.
    Cpal(ThreadOwnedStream),
    /// Core Audio direct implementation (macOS only)
    #[cfg(target_os = "macos")]
    CoreAudio {
        task: Option<tokio::task::JoinHandle<()>>,
    },
}

impl StreamBackend {
    fn stop(&mut self) -> Result<()> {
        match self {
            StreamBackend::Cpal(worker) => worker.stop(),
            #[cfg(target_os = "macos")]
            StreamBackend::CoreAudio { task } => {
                if let Some(task_handle) = task.take() {
                    info!("Aborting Core Audio task...");
                    task_handle.abort();
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    info!("Core Audio task aborted");
                }
                Ok(())
            }
        }
    }
}

impl Drop for StreamBackend {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            warn!("Failed to stop audio backend during drop: {error}");
        }
    }
}

/// Simplified audio stream wrapper with multi-backend support
pub struct AudioStream {
    device: Arc<AudioDevice>,
    backend: StreamBackend,
}

impl AudioStream {
    /// Create a new audio stream for the given device
    pub async fn create(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<Self> {
        // Get current backend from global config
        let backend_type = get_current_backend();
        Self::create_with_backend(device, state, device_type, recording_sender, backend_type).await
    }

    /// Create a new audio stream with explicit backend selection
    pub async fn create_with_backend(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
        backend_type: AudioCaptureBackend,
    ) -> Result<Self> {
        info!(
            "🎵 Stream: Creating audio stream for device: {} with backend: {:?}, device_type: {:?}",
            device.name, backend_type, device_type
        );

        // For system audio devices, use the selected backend
        // For microphone devices, always use CPAL
        #[cfg(target_os = "macos")]
        let use_core_audio =
            device_type == DeviceType::System && backend_type == AudioCaptureBackend::CoreAudio;

        #[cfg(not(target_os = "macos"))]
        let use_core_audio = false;

        #[cfg(target_os = "macos")]
        info!(
            "🎵 Stream: use_core_audio = {}, device_type == System: {}, backend == CoreAudio: {}",
            use_core_audio,
            device_type == DeviceType::System,
            backend_type == AudioCaptureBackend::CoreAudio
        );

        #[cfg(not(target_os = "macos"))]
        info!(
            "🎵 Stream: use_core_audio = {}, device_type == System: {}",
            use_core_audio,
            device_type == DeviceType::System
        );

        #[cfg(target_os = "macos")]
        if use_core_audio {
            info!("🎵 Stream: Using Core Audio backend (cidre) for system audio");
            return Self::create_core_audio_stream(device, state, device_type, recording_sender)
                .await;
        }

        // Default path: use CPAL
        #[cfg(target_os = "macos")]
        let backend_name = if backend_type == AudioCaptureBackend::ScreenCaptureKit {
            "ScreenCaptureKit"
        } else {
            "CPAL (default)"
        };

        #[cfg(not(target_os = "macos"))]
        let backend_name = "CPAL";

        info!(
            "🎵 Stream: Using CPAL backend ({}) for device: {}",
            backend_name, device.name
        );
        Self::create_cpal_stream(device, state, device_type, recording_sender).await
    }

    /// Create a CPAL-based stream (ScreenCaptureKit on macOS)
    async fn create_cpal_stream(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<Self> {
        info!("Creating CPAL stream for device: {}", device.name);

        let owner_device = device.clone();
        let owner_device_name = device.name.clone();
        let stop_device_name = device.name.clone();
        let thread_name = match device_type {
            DeviceType::Microphone => "meetily-cpal-microphone",
            DeviceType::System => "meetily-cpal-system",
            DeviceType::Mixed => "meetily-cpal-mixed",
        }
        .to_string();

        let worker = ThreadOwnedStream::spawn(
            thread_name,
            move || {
                // Device lookup is currently synchronous behind an async API.
                // Resolve it on this same owner thread so neither `Device` nor
                // the resulting `Stream` crosses a thread boundary.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        anyhow::anyhow!("Failed to create audio owner runtime: {error}")
                    })?;
                let (cpal_device, config) =
                    runtime.block_on(get_device_and_config(&owner_device))?;
                drop(runtime);

                info!(
                    "Audio config - Sample rate: {}, Channels: {}, Format: {:?}",
                    config.sample_rate().0,
                    config.channels(),
                    config.sample_format()
                );

                let capture = AudioCapture::new(
                    owner_device,
                    state,
                    config.sample_rate().0,
                    config.channels(),
                    device_type,
                    recording_sender,
                );
                let stream = Self::build_stream(&cpal_device, &config, capture)?;
                stream.play()?;
                info!("CPAL stream started for device: {}", owner_device_name);
                Ok(stream)
            },
            move |stream: Stream| {
                info!("Stopping CPAL stream for device: {}", stop_device_name);
                if let Err(error) = stream.pause() {
                    warn!("Failed to pause stream before drop: {error}");
                }
                drop(stream);
                info!("CPAL stream dropped on its owner thread");
                Ok(())
            },
        )
        .await?;

        Ok(Self {
            device,
            backend: StreamBackend::Cpal(worker),
        })
    }

    /// Create a Core Audio stream (macOS only)
    #[cfg(target_os = "macos")]
    async fn create_core_audio_stream(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<Self> {
        info!(
            "🔊 Stream: Creating Core Audio stream for device: {}",
            device.name
        );

        // Create Core Audio capture
        info!("🔊 Stream: Calling CoreAudioCapture::new()...");
        let capture_impl = CoreAudioCapture::new().map_err(|e| {
            error!("❌ Stream: CoreAudioCapture::new() failed: {}", e);
            anyhow::anyhow!("Failed to create Core Audio capture: {}", e)
        })?;

        info!("✅ Stream: CoreAudioCapture created, calling stream()...");
        let core_stream = capture_impl.stream().map_err(|e| {
            error!("❌ Stream: capture_impl.stream() failed: {}", e);
            anyhow::anyhow!("Failed to create Core Audio stream: {}", e)
        })?;

        let sample_rate = core_stream.sample_rate();
        info!(
            "✅ Stream: Core Audio stream created with sample rate: {} Hz",
            sample_rate
        );

        // Create audio capture processor for pipeline integration
        // CRITICAL: Core Audio tap is MONO (with_mono_global_tap_excluding_processes)
        let capture = AudioCapture::new(
            device.clone(),
            state.clone(),
            sample_rate,
            1, // Core Audio tap is MONO (not stereo!)
            device_type,
            recording_sender,
        );

        // Spawn task to process Core Audio stream samples
        // The stream needs to be polled continuously to produce samples
        let device_name = device.name.clone();
        info!("🔊 Stream: Spawning tokio task to poll Core Audio stream...");
        let task = tokio::spawn({
            let capture = capture.clone();
            let mut stream = core_stream;

            async move {
                use futures_util::StreamExt;

                let mut buffer = Vec::new();
                let mut frame_count = 0;
                let frames_per_chunk = 1024; // Process in chunks of 1024 samples

                info!(
                    "✅ Stream: Core Audio processing task started for {}",
                    device_name
                );

                let mut _sample_count = 0u64;
                while let Some(sample) = stream.next().await {
                    _sample_count += 1;
                    // if _sample_count % 48000 == 0 {
                    //     info!("📊 Stream: Received {} samples from Core Audio stream", _sample_count);
                    // }

                    buffer.push(sample);
                    frame_count += 1;

                    // Process when we have enough samples
                    if frame_count >= frames_per_chunk {
                        capture.process_audio_data(&buffer);
                        buffer.clear();
                        frame_count = 0;
                    }
                }

                // Process any remaining samples
                if !buffer.is_empty() {
                    capture.process_audio_data(&buffer);
                }

                info!(
                    "⚠️ Stream: Core Audio processing task ended for {}",
                    device_name
                );
            }
        });

        info!(
            "✅ Stream: Core Audio stream fully initialized for device: {}",
            device.name
        );

        Ok(Self {
            device: device.clone(),
            backend: StreamBackend::CoreAudio { task: Some(task) },
        })
    }

    /// Build stream based on sample format
    fn build_stream(
        device: &Device,
        config: &SupportedStreamConfig,
        capture: AudioCapture,
    ) -> Result<Stream> {
        let config_copy = config.clone();

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                let capture_clone = capture.clone();
                device.build_input_stream(
                    &config_copy.into(),
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        capture.process_audio_data(data);
                    },
                    move |err| {
                        capture_clone.handle_stream_error(err);
                    },
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                let capture_clone = capture.clone();
                device.build_input_stream(
                    &config_copy.into(),
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let f32_data: Vec<f32> = data
                            .iter()
                            .map(|&sample| sample as f32 / i16::MAX as f32)
                            .collect();
                        capture.process_audio_data(&f32_data);
                    },
                    move |err| {
                        capture_clone.handle_stream_error(err);
                    },
                    None,
                )?
            }
            cpal::SampleFormat::I32 => {
                let capture_clone = capture.clone();
                device.build_input_stream(
                    &config_copy.into(),
                    move |data: &[i32], _: &cpal::InputCallbackInfo| {
                        let f32_data: Vec<f32> = data
                            .iter()
                            .map(|&sample| sample as f32 / i32::MAX as f32)
                            .collect();
                        capture.process_audio_data(&f32_data);
                    },
                    move |err| {
                        capture_clone.handle_stream_error(err);
                    },
                    None,
                )?
            }
            cpal::SampleFormat::I8 => {
                let capture_clone = capture.clone();
                device.build_input_stream(
                    &config_copy.into(),
                    move |data: &[i8], _: &cpal::InputCallbackInfo| {
                        let f32_data: Vec<f32> = data
                            .iter()
                            .map(|&sample| sample as f32 / i8::MAX as f32)
                            .collect();
                        capture.process_audio_data(&f32_data);
                    },
                    move |err| {
                        capture_clone.handle_stream_error(err);
                    },
                    None,
                )?
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Unsupported sample format: {:?}",
                    config.sample_format()
                ));
            }
        };

        Ok(stream)
    }

    /// Get device info
    pub fn device(&self) -> &AudioDevice {
        &self.device
    }

    /// Stop the stream
    pub fn stop(mut self) -> Result<()> {
        info!("Stopping audio stream for device: {}", self.device.name);
        self.backend.stop()?;
        info!("Audio stream stopped and device reference dropped");
        Ok(())
    }
}

/// Audio stream manager for handling multiple streams
pub struct AudioStreamManager {
    microphone_stream: Option<AudioStream>,
    system_stream: Option<AudioStream>,
    state: Arc<RecordingState>,
    startup_failures: Vec<AudioStreamStartupFailure>,
}

impl AudioStreamManager {
    pub fn new(state: Arc<RecordingState>) -> Self {
        Self {
            microphone_stream: None,
            system_stream: None,
            state,
            startup_failures: Vec::new(),
        }
    }

    /// Start audio streams for the given devices
    pub async fn start_streams(
        &mut self,
        microphone_device: Option<Arc<AudioDevice>>,
        system_device: Option<Arc<AudioDevice>>,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<()> {
        use super::capture::get_current_backend;
        let backend = get_current_backend();
        info!("🎙️ Starting audio streams with backend: {:?}", backend);

        self.startup_failures.clear();

        // Start microphone stream
        if let Some(mic_device) = microphone_device {
            let device_label = mic_device.name.clone();
            if let Err(error) = self
                .start_microphone_stream(mic_device, recording_sender.clone())
                .await
            {
                error!("❌ Failed to create microphone stream: {}", error);
                self.startup_failures.push(AudioStreamStartupFailure {
                    device_type: DeviceType::Microphone,
                    device_label,
                    detail: error.to_string(),
                });
            }
        } else {
            info!("ℹ️ No microphone device specified, skipping microphone stream");
        }

        // Start system audio stream
        if let Some(sys_device) = system_device {
            let device_label = sys_device.name.clone();
            if let Err(error) = self
                .start_system_audio_stream(sys_device, recording_sender)
                .await
            {
                warn!("⚠️ Failed to create system audio stream: {}", error);
                self.startup_failures.push(AudioStreamStartupFailure {
                    device_type: DeviceType::System,
                    device_label,
                    detail: error.to_string(),
                });
            }
        } else {
            info!("ℹ️ No system device specified, skipping system audio stream");
        }

        // A dual-source session remains useful when either source cannot start.
        // Fail only when there is no healthy route at all.
        if self.microphone_stream.is_none() && self.system_stream.is_none() {
            let details = if self.startup_failures.is_empty() {
                "no devices were selected".to_string()
            } else {
                self.startup_failures
                    .iter()
                    .map(|failure| {
                        format!(
                            "{} '{}': {}",
                            match failure.device_type {
                                DeviceType::Microphone => "microphone",
                                DeviceType::System => "system audio",
                                DeviceType::Mixed => "mixed audio",
                            },
                            failure.device_label,
                            failure.detail
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            return Err(anyhow::anyhow!(
                "No audio streams could be created ({details})"
            ));
        }

        if !self.startup_failures.is_empty() {
            warn!(
                "Continuing with {} healthy audio route(s); {} selected route(s) were unavailable",
                self.active_stream_count(),
                self.startup_failures.len()
            );
        }

        Ok(())
    }

    async fn start_microphone_stream(
        &mut self,
        microphone_device: Arc<AudioDevice>,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<()> {
        if self.microphone_stream.is_some() {
            return Err(anyhow::anyhow!("Microphone stream is already active"));
        }

        info!(
            "🎤 Creating microphone stream: {} (always uses CPAL)",
            microphone_device.name
        );
        let stream = AudioStream::create(
            microphone_device.clone(),
            self.state.clone(),
            DeviceType::Microphone,
            recording_sender,
        )
        .await?;

        self.state.set_microphone_device(microphone_device);
        self.microphone_stream = Some(stream);
        info!("✅ Microphone stream created successfully");
        Ok(())
    }

    async fn start_system_audio_stream(
        &mut self,
        system_device: Arc<AudioDevice>,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<()> {
        if self.system_stream.is_some() {
            return Err(anyhow::anyhow!("System audio stream is already active"));
        }

        let backend = get_current_backend();
        info!(
            "🔊 Creating system audio stream: {} (backend: {:?})",
            system_device.name, backend
        );
        let stream = AudioStream::create(
            system_device.clone(),
            self.state.clone(),
            DeviceType::System,
            recording_sender,
        )
        .await?;

        self.state.set_system_device(system_device);
        self.system_stream = Some(stream);
        info!("✅ System audio stream created with {:?} backend", backend);
        Ok(())
    }

    /// Stop only the microphone route, leaving system audio uninterrupted.
    pub fn stop_microphone_stream(&mut self) -> Result<()> {
        if let Some(stream) = self.microphone_stream.take() {
            stream.stop()?;
            info!("Microphone stream stopped; system audio route was preserved");
        }
        Ok(())
    }

    /// Stop only the system-audio route, leaving the microphone uninterrupted.
    pub fn stop_system_audio_stream(&mut self) -> Result<()> {
        if let Some(stream) = self.system_stream.take() {
            stream.stop()?;
            info!("System audio stream stopped; microphone route was preserved");
        }
        Ok(())
    }

    /// Replace only the microphone route after a disconnect.
    pub async fn rebuild_microphone_stream(
        &mut self,
        microphone_device: Arc<AudioDevice>,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<()> {
        self.stop_microphone_stream()?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.start_microphone_stream(microphone_device, recording_sender)
            .await
    }

    /// Replace only the system-audio route after a disconnect.
    pub async fn rebuild_system_audio_stream(
        &mut self,
        system_device: Arc<AudioDevice>,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<()> {
        self.stop_system_audio_stream()?;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        self.start_system_audio_stream(system_device, recording_sender)
            .await
    }

    /// Stop all audio streams
    pub fn stop_streams(&mut self) -> Result<()> {
        info!("Stopping all audio streams");

        let mut errors = Vec::new();

        if let Err(error) = self.stop_microphone_stream() {
            error!("Failed to stop microphone stream: {}", error);
            errors.push(error);
        }

        if let Err(error) = self.stop_system_audio_stream() {
            error!("Failed to stop system stream: {}", error);
            errors.push(error);
        }

        if !errors.is_empty() {
            Err(anyhow::anyhow!("Failed to stop some streams: {:?}", errors))
        } else {
            info!("All audio streams stopped successfully");
            Ok(())
        }
    }

    /// Get stream count
    pub fn active_stream_count(&self) -> usize {
        let mut count = 0;
        if self.microphone_stream.is_some() {
            count += 1;
        }
        if self.system_stream.is_some() {
            count += 1;
        }
        count
    }

    /// Check if any streams are active
    pub fn has_active_streams(&self) -> bool {
        self.microphone_stream.is_some() || self.system_stream.is_some()
    }

    pub fn has_microphone_stream(&self) -> bool {
        self.microphone_stream.is_some()
    }

    pub fn has_system_audio_stream(&self) -> bool {
        self.system_stream.is_some()
    }

    pub fn startup_failures(&self) -> &[AudioStreamStartupFailure] {
        &self.startup_failures
    }
}

impl Drop for AudioStreamManager {
    fn drop(&mut self) {
        if let Err(e) = self.stop_streams() {
            error!("Error stopping streams during drop: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::sync::Mutex;

    struct ThreadBoundProbe {
        owner: std::thread::ThreadId,
        dropped_on: Arc<Mutex<Option<std::thread::ThreadId>>>,
        _not_send: Rc<()>,
    }

    impl Drop for ThreadBoundProbe {
        fn drop(&mut self) {
            *self.dropped_on.lock().expect("probe lock poisoned") =
                Some(std::thread::current().id());
        }
    }

    #[test]
    fn stream_control_types_are_send_without_unsafe_overrides() {
        fn assert_send<T: Send>() {}

        assert_send::<ThreadOwnedStream>();
        assert_send::<AudioStream>();
        assert_send::<AudioStreamManager>();
    }

    #[tokio::test]
    async fn non_send_resource_is_created_and_dropped_on_its_owner_thread() {
        let caller_thread = std::thread::current().id();
        let created_on = Arc::new(Mutex::new(None));
        let dropped_on = Arc::new(Mutex::new(None));
        let created_on_worker = created_on.clone();
        let dropped_on_worker = dropped_on.clone();

        let mut worker = ThreadOwnedStream::spawn(
            "meetily-thread-owner-test".to_string(),
            move || {
                let owner = std::thread::current().id();
                *created_on_worker.lock().expect("probe lock poisoned") = Some(owner);
                Ok(ThreadBoundProbe {
                    owner,
                    dropped_on: dropped_on_worker,
                    _not_send: Rc::new(()),
                })
            },
            |probe: ThreadBoundProbe| {
                assert_eq!(probe.owner, std::thread::current().id());
                drop(probe);
                Ok(())
            },
        )
        .await
        .expect("owner thread should start");

        worker.stop().expect("owner thread should stop cleanly");

        let created_on = created_on.lock().expect("probe lock poisoned").unwrap();
        let dropped_on = dropped_on.lock().expect("probe lock poisoned").unwrap();
        assert_ne!(created_on, caller_thread);
        assert_eq!(created_on, dropped_on);
    }

    #[test]
    fn stopping_an_absent_route_does_not_affect_the_other_slot() {
        let state = RecordingState::new();
        let mut manager = AudioStreamManager::new(state);

        assert!(manager.stop_microphone_stream().is_ok());
        assert!(manager.stop_system_audio_stream().is_ok());
        assert!(!manager.has_microphone_stream());
        assert!(!manager.has_system_audio_stream());
        assert!(!manager.has_active_streams());
        assert!(manager.startup_failures().is_empty());
    }
}
