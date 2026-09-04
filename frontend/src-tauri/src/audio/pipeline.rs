use super::batch_processor::AudioMetricsBatcher;
use crate::batch_audio_metric;
use anyhow::Result;
use log::{debug, error, info, warn};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::audio_processing::{
    audio_to_mono, HighPassFilter, LoudnessNormalizer, NoiseSuppressionProcessor,
};
use super::devices::AudioDevice;
use super::gap_health::GapHealthTracker;
use super::recording_state::{AudioChunk, AudioError, DeviceType, RecordingState};
use super::synchronizer::{
    AlignedAudioWindow, AudioSynchronizer, AudioTrack, CaptureTopology, GapReason,
};
use super::timeline_event::record_audio_timeline_event;
use super::transcription::streaming::{
    StreamingAsrCommand, StreamingAudioFrame, StreamingFlushReason,
};
use super::transcription::AudioSource;
use super::vad::ContinuousVadProcessor;

/// Ring buffer for synchronized audio mixing
/// Accumulates samples from mic and system streams until we have aligned windows
struct AudioMixerRingBuffer {
    mic_buffer: VecDeque<f32>,
    system_buffer: VecDeque<f32>,
    window_size_samples: usize, // Fixed mixing window (e.g., 50ms)
    max_buffer_size: usize,     // Safety limit (e.g., 100ms)
}

impl AudioMixerRingBuffer {
    fn new(sample_rate: u32) -> Self {
        // Use 50ms windows for mixing
        let window_ms = 600.0;
        let window_size_samples = (sample_rate as f32 * window_ms / 1000.0) as usize;

        // CRITICAL FIX: Increase max buffer to 400ms for system audio stability
        // System audio (especially Core Audio on macOS) can have significant jitter
        // due to sample-by-sample streaming → batching → channel transmission
        // Accounts for: RNNoise buffering + Core Audio jitter + processing delays
        let max_buffer_size = window_size_samples * 8; // 400ms (was 200ms)

        info!(
            "🔊 Ring buffer initialized: window={}ms ({} samples), max={}ms ({} samples)",
            window_ms,
            window_size_samples,
            window_ms * 8.0,
            max_buffer_size
        );

        Self {
            mic_buffer: VecDeque::with_capacity(max_buffer_size),
            system_buffer: VecDeque::with_capacity(max_buffer_size),
            window_size_samples,
            max_buffer_size,
        }
    }

    fn add_samples(&mut self, device_type: DeviceType, samples: Vec<f32>) {
        // Log buffer health periodically for diagnostics
        static mut SAMPLE_COUNTER: u64 = 0;
        unsafe {
            SAMPLE_COUNTER += 1;
            if SAMPLE_COUNTER % 200 == 0 {
                debug!(
                    "📊 Ring buffer status: mic={} samples, sys={} samples (max={})",
                    self.mic_buffer.len(),
                    self.system_buffer.len(),
                    self.max_buffer_size
                );
            }
        }

        match device_type {
            DeviceType::Microphone => self.mic_buffer.extend(samples),
            DeviceType::System => self.system_buffer.extend(samples),
            DeviceType::Mixed => {
                warn!("Derived mixed audio cannot be inserted into the legacy capture ring")
            }
        }

        // CRITICAL FIX: Add warnings before dropping samples
        // This helps diagnose timing issues in production
        if self.mic_buffer.len() > self.max_buffer_size {
            warn!(
                "⚠️ Microphone buffer overflow: {} > {} samples, dropping oldest {} samples",
                self.mic_buffer.len(),
                self.max_buffer_size,
                self.mic_buffer.len() - self.max_buffer_size
            );
        }
        if self.system_buffer.len() > self.max_buffer_size {
            error!("🔴 SYSTEM AUDIO BUFFER OVERFLOW: {} > {} samples, dropping {} samples - THIS CAUSES DISTORTION!",
                  self.system_buffer.len(), self.max_buffer_size,
                  self.system_buffer.len() - self.max_buffer_size);
        }

        // Safety: prevent buffer overflow (keep only last 200ms)
        while self.mic_buffer.len() > self.max_buffer_size {
            self.mic_buffer.pop_front();
        }
        while self.system_buffer.len() > self.max_buffer_size {
            self.system_buffer.pop_front();
        }
    }

    fn can_mix(&self) -> bool {
        self.mic_buffer.len() >= self.window_size_samples
            || self.system_buffer.len() >= self.window_size_samples
    }

    fn extract_window(&mut self) -> Option<(Vec<f32>, Vec<f32>)> {
        if !self.can_mix() {
            return None;
        }

        // Extract mic window with zero-padding for incomplete buffers
        // Zero-padding (silence) is preferred over last-sample-hold to prevent artifacts

        // Extract mic window (or pad with zeros if insufficient data)
        let mic_window = if self.mic_buffer.len() >= self.window_size_samples {
            // Enough mic data - drain window
            self.mic_buffer.drain(0..self.window_size_samples).collect()
        } else if !self.mic_buffer.is_empty() {
            // Some mic data but not enough - consume all + pad with zeros
            let available: Vec<f32> = self.mic_buffer.drain(..).collect();
            let mut padded = Vec::with_capacity(self.window_size_samples);
            padded.extend_from_slice(&available);

            // Use zero-padding (silence) to prevent repetition artifacts
            // Zero-padding is inaudible at 48kHz sample rate
            padded.resize(self.window_size_samples, 0.0);

            padded
        } else {
            // No mic data - return silence
            vec![0.0; self.window_size_samples]
        };

        // Extract system window (or pad with zeros if insufficient data)
        let sys_window = if self.system_buffer.len() >= self.window_size_samples {
            // Enough system data - drain window
            self.system_buffer
                .drain(0..self.window_size_samples)
                .collect()
        } else if !self.system_buffer.is_empty() {
            // Some system data but not enough - consume all + pad with zeros
            let available: Vec<f32> = self.system_buffer.drain(..).collect();
            let mut padded = Vec::with_capacity(self.window_size_samples);
            padded.extend_from_slice(&available);

            // Use zero-padding (silence) to prevent repetition artifacts
            // Zero-padding is inaudible at 48kHz sample rate
            padded.resize(self.window_size_samples, 0.0);

            padded
        } else {
            // No system data - return silence
            vec![0.0; self.window_size_samples]
        };

        Some((mic_window, sys_window))
    }
}

/// Simple audio mixer without aggressive ducking
/// Combines mic + system audio with basic clipping prevention
struct ProfessionalAudioMixer;

impl ProfessionalAudioMixer {
    fn new(_sample_rate: u32) -> Self {
        Self
    }

    fn mix_window(&mut self, mic_window: &[f32], sys_window: &[f32]) -> Vec<f32> {
        // Handle different lengths (already padded by extract_window, but defensive)
        let max_len = mic_window.len().max(sys_window.len());
        let mut mixed = Vec::with_capacity(max_len);

        // Professional mixing with soft scaling to prevent distortion
        // Uses proportional scaling instead of hard clamping to avoid artifacts
        for i in 0..max_len {
            let mic = mic_window.get(i).copied().unwrap_or(0.0);
            let sys = sys_window.get(i).copied().unwrap_or(0.0);

            // Pre-scale system audio to 70% to leave headroom
            // This prevents constant soft scaling which can cause pumping artifacts
            // Mic is normalized to -23 LUFS (already optimal), system needs reduction
            let sys_scaled = sys * 1.0;
            let _mic_scaled = mic * 0.8; // Reserved for future mic scaling

            // Sum without ducking - mic stays at full volume, system slightly reduced
            let sum = mic + sys_scaled;

            // CRITICAL FIX: Soft scaling prevents distortion artifacts
            // If the sum would exceed ±1.0, scale down PROPORTIONALLY
            // This avoids hard clipping distortion that sounds like "radio breaks"
            let sum_abs = sum.abs();
            let mixed_sample = if sum_abs > 1.0 {
                // Scale down to fit within ±1.0
                sum / sum_abs
            } else {
                sum
            };

            mixed.push(mixed_sample);
        }

        mixed
    }
}

/// Simplified audio capture without broadcast channels
#[derive(Clone)]
pub struct AudioCapture {
    device: Arc<AudioDevice>,
    state: Arc<RecordingState>,
    sample_rate: u32, // Original device sample rate
    channels: u16,
    chunk_counter: Arc<std::sync::atomic::AtomicU64>,
    source_frame_cursor: Arc<std::sync::atomic::AtomicU64>,
    device_type: DeviceType,
    recording_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
    needs_resampling: bool, // Flag if resampling is required
    // CRITICAL FIX: Persistent resampler to preserve energy across chunks
    resampler: Arc<std::sync::Mutex<Option<SincFixedIn<f32>>>>,
    // Buffering for variable-size chunks → fixed-size resampler input
    resampler_input_buffer: Arc<std::sync::Mutex<Vec<f32>>>,
    resampler_chunk_size: usize, // Fixed chunk size for resampler (512 samples)
    // Audio enhancement processors (microphone only)
    noise_suppressor: Arc<std::sync::Mutex<Option<NoiseSuppressionProcessor>>>,
    high_pass_filter: Arc<std::sync::Mutex<Option<HighPassFilter>>>,
    // EBU R128 normalizer for microphone audio (per-device, stateful)
    normalizer: Arc<std::sync::Mutex<Option<LoudnessNormalizer>>>,
    // Note: Using global recording timestamp for synchronization
}

impl AudioCapture {
    pub fn new(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        sample_rate: u32,
        channels: u16,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
    ) -> Self {
        // CRITICAL FIX: Detect if resampling is needed
        // Pipeline expects 48kHz, but Bluetooth devices often report 8kHz, 16kHz, or 44.1kHz
        const TARGET_SAMPLE_RATE: u32 = 48000;
        let needs_resampling = sample_rate != TARGET_SAMPLE_RATE;

        // Detect device kind (Bluetooth vs Wired) for adaptive processing
        // Use reasonable defaults for buffer size (512 samples is typical)
        let device_kind =
            super::device_detection::InputDeviceKind::detect(&device.name, 512, sample_rate);

        if needs_resampling {
            warn!("⚠️ SAMPLE RATE MISMATCH DETECTED ⚠️");
            warn!(
                "🔄 [{:?}] Audio device '{}' ({:?}) reports {} Hz (pipeline expects {} Hz)",
                device_type, device.name, device_kind, sample_rate, TARGET_SAMPLE_RATE
            );
            warn!(
                "🔄 Automatic resampling will be applied: {} Hz → {} Hz",
                sample_rate, TARGET_SAMPLE_RATE
            );

            // Log which resampling strategy will be used
            let ratio = TARGET_SAMPLE_RATE as f64 / sample_rate as f64;
            let strategy = if ratio >= 2.0 {
                "High-quality upsampling (sinc_len=512, Cubic interpolation)"
            } else if ratio >= 1.5 {
                "Moderate upsampling (sinc_len=384, Cubic)"
            } else if ratio > 1.0 {
                "Small upsampling (sinc_len=256, Linear)"
            } else if ratio <= 0.5 {
                "Anti-aliased downsampling (sinc_len=512, Cubic)"
            } else {
                "Moderate downsampling (sinc_len=384, Linear)"
            };
            info!("   Resampling strategy: {}", strategy);
        } else {
            info!(
                "✅ [{:?}] Audio device '{}' ({:?}) uses {} Hz (matches pipeline)",
                device_type, device.name, device_kind, sample_rate
            );
        }

        // Initialize audio enhancement processors for MICROPHONE ONLY
        // System audio doesn't need enhancement (already clean)
        let (noise_suppressor, high_pass_filter, normalizer) = if matches!(
            device_type,
            DeviceType::Microphone
        ) {
            // Initialize noise suppression (RNNoise) at 48kHz - CONDITIONAL based on flag
            let ns = if super::ffmpeg_mixer::RNNOISE_APPLY_ENABLED {
                match NoiseSuppressionProcessor::new(TARGET_SAMPLE_RATE) {
                    Ok(processor) => {
                        info!("✅ RNNoise noise suppression ENABLED for microphone '{}' (10-15 dB reduction)", device.name);
                        Some(processor)
                    }
                    Err(e) => {
                        warn!("⚠️ Failed to create noise suppressor: {}, continuing without noise suppression", e);
                        None
                    }
                }
            } else {
                info!("ℹ️ RNNoise noise suppression DISABLED for microphone '{}' (flag: RNNOISE_APPLY_ENABLED=false)", device.name);
                info!("   Whisper handles noise well internally - RNNoise is optional");
                None
            };

            // Initialize high-pass filter (removes rumble below 80 Hz)
            let hpf = {
                let filter = HighPassFilter::new(TARGET_SAMPLE_RATE, 80.0);
                info!(
                    "✅ High-pass filter initialized for microphone '{}' (cutoff: 80 Hz)",
                    device.name
                );
                Some(filter)
            };

            // Initialize EBU R128 normalizer (professional loudness standard)
            let norm = match LoudnessNormalizer::new(1, TARGET_SAMPLE_RATE) {
                Ok(normalizer) => {
                    info!(
                        "✅ EBU R128 normalizer initialized for microphone '{}' (target: -23 LUFS)",
                        device.name
                    );
                    Some(normalizer)
                }
                Err(e) => {
                    warn!(
                        "⚠️ Failed to create normalizer for microphone: {}, normalization disabled",
                        e
                    );
                    None
                }
            };

            (ns, hpf, norm)
        } else {
            // System audio: no enhancement needed
            info!(
                "ℹ️ System audio '{}' captured raw (no enhancement)",
                device.name
            );
            (None, None, None)
        };

        // CRITICAL FIX: Initialize persistent resampler to preserve energy across chunks
        // Creating a new resampler per chunk causes energy amplification and incorrect output sizes
        // Use fixed chunk size of 512 samples with buffering for variable-size input
        const RESAMPLER_CHUNK_SIZE: usize = 512;

        let resampler = if needs_resampling {
            let ratio = TARGET_SAMPLE_RATE as f64 / sample_rate as f64;

            // Adaptive parameters based on sample rate ratio (same logic as resample_audio)
            let (sinc_len, interpolation_type, oversampling) = if ratio >= 2.0 {
                (512, SincInterpolationType::Cubic, 512)
            } else if ratio >= 1.5 {
                (384, SincInterpolationType::Cubic, 384)
            } else if ratio > 1.0 {
                (256, SincInterpolationType::Linear, 256)
            } else if ratio <= 0.5 {
                (512, SincInterpolationType::Cubic, 512)
            } else {
                (384, SincInterpolationType::Linear, 384)
            };

            let params = SincInterpolationParameters {
                sinc_len,
                f_cutoff: 0.95,
                interpolation: interpolation_type,
                oversampling_factor: oversampling,
                window: WindowFunction::BlackmanHarris2,
            };

            match SincFixedIn::<f32>::new(
                ratio,
                2.0, // Maximum relative deviation
                params,
                RESAMPLER_CHUNK_SIZE,
                1, // Mono
            ) {
                Ok(resampler) => {
                    info!(
                        "✅ Persistent resampler initialized for '{}' ({}Hz → {}Hz, chunk_size={})",
                        device.name, sample_rate, TARGET_SAMPLE_RATE, RESAMPLER_CHUNK_SIZE
                    );
                    info!("   Buffering enabled for variable-size chunks (e.g., 320, 512, 1024, etc.)");
                    Some(resampler)
                }
                Err(e) => {
                    warn!(
                        "⚠️ Failed to create persistent resampler: {}, will use fallback",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        let initial_source_frame = state.get_capture_start_frame(TARGET_SAMPLE_RATE);

        Self {
            device,
            state,
            sample_rate,
            channels,
            chunk_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            // A recreated source resumes at the already committed media frame;
            // stale callbacks from the previous generation are discarded by
            // the synchronizer once that range has been committed.
            source_frame_cursor: Arc::new(std::sync::atomic::AtomicU64::new(initial_source_frame)),
            device_type,
            recording_sender,
            needs_resampling,
            resampler: Arc::new(std::sync::Mutex::new(resampler)),
            resampler_input_buffer: Arc::new(std::sync::Mutex::new(Vec::with_capacity(
                RESAMPLER_CHUNK_SIZE * 2,
            ))),
            resampler_chunk_size: RESAMPLER_CHUNK_SIZE,
            noise_suppressor: Arc::new(std::sync::Mutex::new(noise_suppressor)),
            high_pass_filter: Arc::new(std::sync::Mutex::new(high_pass_filter)),
            normalizer: Arc::new(std::sync::Mutex::new(normalizer)),
            // Using global recording time for sync
        }
    }

    /// Process audio data directly from callback
    pub fn process_audio_data(&self, data: &[f32]) {
        // Check if still recording
        if !self.state.is_active() {
            return;
        }

        // Convert to mono if needed
        let mut mono_data = if self.channels > 1 {
            audio_to_mono(data, self.channels)
        } else {
            data.to_vec()
        };

        // CRITICAL FIX: Resample to 48kHz if device uses different sample rate
        // This fixes Bluetooth devices (like Sony WH-1000XM4) that report 16kHz or 44.1kHz
        // Without this, audio is sped up 3x and VAD fails
        //
        // IMPORTANT: Uses PERSISTENT resampler with BUFFERING to preserve energy across chunks
        // Creating a new resampler per chunk causes energy amplification (173.5% RMS)
        // Buffering handles variable chunk sizes (320, 512, 1024, etc.) by accumulating to fixed 512-sample chunks
        const TARGET_SAMPLE_RATE: u32 = 48000;
        if self.needs_resampling {
            let before_len = mono_data.len();
            let before_rms = if !mono_data.is_empty() {
                (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt()
            } else {
                0.0
            };

            // Use persistent resampler with buffering to handle variable chunk sizes
            let mut resampled_output = Vec::new();
            let mut used_persistent_resampler = false;

            if let Ok(mut buffer_lock) = self.resampler_input_buffer.lock() {
                // Add new samples to buffer
                buffer_lock.extend_from_slice(&mono_data);

                // Process complete chunks through the resampler
                if let Ok(mut resampler_lock) = self.resampler.lock() {
                    if let Some(ref mut resampler) = *resampler_lock {
                        used_persistent_resampler = true;

                        // Process as many complete chunks as we have
                        while buffer_lock.len() >= self.resampler_chunk_size {
                            // Extract exactly chunk_size samples
                            let chunk: Vec<f32> =
                                buffer_lock.drain(0..self.resampler_chunk_size).collect();

                            // Rubato expects input as Vec<Vec<f32>> (one Vec per channel)
                            let waves_in = vec![chunk];

                            match resampler.process(&waves_in, None) {
                                Ok(mut waves_out) => {
                                    if let Some(output) = waves_out.pop() {
                                        resampled_output.extend_from_slice(&output);
                                    }
                                }
                                Err(e) => {
                                    warn!("⚠️ Persistent resampler processing failed: {}", e);
                                    used_persistent_resampler = false;
                                    break;
                                }
                            }
                        }
                        // Remaining samples in buffer will be processed in next iteration
                    }
                }
            }

            // CRITICAL: Only update mono_data if we got output from persistent resampler
            // If buffer is accumulating (< 512 samples), skip this chunk - data is safely buffered
            // and will be processed in next iteration with proper resampling
            let has_resampled_output = !resampled_output.is_empty();

            if has_resampled_output {
                mono_data = resampled_output;
            } else if !used_persistent_resampler {
                // Only fallback if persistent resampler is not available at all
                mono_data = super::audio_processing::resample_audio(
                    &mono_data,
                    self.sample_rate,
                    TARGET_SAMPLE_RATE,
                );
            } else {
                // Buffering: samples are accumulating in buffer, waiting for 512-sample chunk
                // Don't send partial/unprocessed data - return early
                // Audio is NOT lost - it's in the buffer and will be processed next iteration
                return;
            }

            // Log resampling only occasionally to avoid spam
            let chunk_id = self.chunk_counter.load(std::sync::atomic::Ordering::SeqCst);
            if chunk_id % 100 == 0 && has_resampled_output {
                let after_len = mono_data.len();
                let after_rms = if !mono_data.is_empty() {
                    (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt()
                } else {
                    0.0
                };
                let ratio = TARGET_SAMPLE_RATE as f64 / self.sample_rate as f64;
                let rms_preservation = if before_rms > 0.0 {
                    (after_rms / before_rms) * 100.0
                } else {
                    100.0
                };

                let buffer_size = if let Ok(buf) = self.resampler_input_buffer.lock() {
                    buf.len()
                } else {
                    0
                };

                info!(
                    "🔄 [{:?}] Persistent buffered resampler: {}Hz → {}Hz (ratio: {:.2}x)",
                    self.device_type, self.sample_rate, TARGET_SAMPLE_RATE, ratio
                );
                info!(
                    "   Chunk {}: {} → {} samples, RMS preservation: {:.1}%, buffer: {}",
                    chunk_id, before_len, after_len, rms_preservation, buffer_size
                );
            }
        }

        // AUDIO ENHANCEMENT PIPELINE (Microphone Only)
        // Processing order is critical: high-pass → noise suppression → normalization
        // This ensures noise is removed before being amplified by the normalizer
        if matches!(self.device_type, DeviceType::Microphone) {
            // STEP 1: Apply high-pass filter to remove low-frequency rumble (< 80 Hz)
            if let Ok(mut hpf_lock) = self.high_pass_filter.lock() {
                if let Some(ref mut filter) = *hpf_lock {
                    mono_data = filter.process(&mono_data);
                }
            }

            // STEP 2: Apply RNNoise noise suppression (10-15 dB reduction) - CONDITIONAL
            if super::ffmpeg_mixer::RNNOISE_APPLY_ENABLED {
                if let Ok(mut ns_lock) = self.noise_suppressor.lock() {
                    if let Some(ref mut suppressor) = *ns_lock {
                        let before_len = mono_data.len();
                        mono_data = suppressor.process(&mono_data);
                        let after_len = mono_data.len();

                        // CRITICAL MONITORING: Track buffer health
                        let chunk_id = self.chunk_counter.load(std::sync::atomic::Ordering::SeqCst);
                        if chunk_id % 100 == 0 {
                            let buffered = suppressor.buffered_samples();
                            let length_delta = (before_len as i32 - after_len as i32).abs();

                            debug!("🔇 Noise suppression health: in={}, out={}, delta={}, buffered={}, RMS={:.4}",
                                   before_len, after_len, length_delta, buffered,
                                   if !mono_data.is_empty() {
                                       (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt()
                                   } else { 0.0 });

                            // WARN if accumulating samples (potential latency buildup)
                            if buffered > 1000 {
                                warn!("⚠️ RNNoise accumulating samples: {} buffered (potential latency issue!)",
                                      buffered);
                            }

                            // WARN if significant length mismatch
                            if length_delta > 50 {
                                warn!(
                                    "⚠️ RNNoise length mismatch: input={} output={} (delta={})",
                                    before_len, after_len, length_delta
                                );
                            }
                        }
                    }
                }
            }

            // STEP 3: Apply EBU R128 normalization (professional loudness standard)
            if let Ok(mut normalizer_lock) = self.normalizer.lock() {
                if let Some(ref mut normalizer) = *normalizer_lock {
                    mono_data = normalizer.normalize_loudness(&mono_data);

                    // Log normalization occasionally for debugging
                    let chunk_id = self.chunk_counter.load(std::sync::atomic::Ordering::SeqCst);
                    if chunk_id % 200 == 0 && !mono_data.is_empty() {
                        let rms = (mono_data.iter().map(|&x| x * x).sum::<f32>()
                            / mono_data.len() as f32)
                            .sqrt();
                        let peak = mono_data.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
                        debug!(
                            "🎤 After normalization chunk {}: RMS={:.4}, Peak={:.4}",
                            chunk_id, rms, peak
                        );
                    }
                }
            }
        }

        // Create audio chunk with stream-specific timestamp (get ID first for logging)
        let chunk_id = self
            .chunk_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // RAW AUDIO: No gain applied here - will be applied AFTER mixing
        // This prevents amplifying system audio bleed-through in the microphone

        // DIAGNOSTIC: Log audio levels for debugging (especially mic issues)
        // if chunk_id % 100 == 0 && !mono_data.is_empty() {
        //     let raw_rms = (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt();
        //     let raw_peak = mono_data.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);

        //         info!("🎙️ [{:?}] Chunk {} - Raw: RMS={:.6}, Peak={:.6}",
        //               self.device_type, chunk_id, raw_rms, raw_peak);

        //     // Warn if microphone is completely silent
        //     if matches!(self.device_type, DeviceType::Microphone) && raw_rms == 0.0 && raw_peak == 0.0 {
        //         warn!("⚠️ Microphone producing ZERO audio - check permissions or hardware!");
        //     }
        // }
        // else if chunk_id % 100 == 0 && matches!(self.device_type, DeviceType::System) {
        //     let raw_rms = (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt();
        //     let raw_peak = mono_data.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        //     info!("🔊 [{:?}] Chunk {} - Raw: RMS={:.6}, Peak={:.6}",
        //       self.device_type, chunk_id, raw_rms, raw_peak);

        //     // Warn if system audio is completely silent
        //     if raw_rms == 0.0 && raw_peak == 0.0 {
        //         warn!("⚠️ System audio producing ZERO audio - check permissions or hardware!");
        //     }
        // }

        let start_frame = self
            .source_frame_cursor
            .fetch_add(mono_data.len() as u64, std::sync::atomic::Ordering::SeqCst);
        let end_frame = start_frame.saturating_add(mono_data.len() as u64);
        let timestamp = start_frame as f64 / 48_000.0;

        // RAW AUDIO CHUNK: No gain applied - will be mixed and gained downstream
        // Use 48kHz if we resampled, otherwise use original rate
        let audio_chunk = AudioChunk {
            data: mono_data, // Raw audio (resampled if needed), no gain yet
            sample_rate: if self.needs_resampling {
                48000
            } else {
                self.sample_rate
            },
            timestamp,
            chunk_id,
            device_type: self.device_type.clone(),
            start_frame: Some(start_frame),
            end_frame: Some(end_frame),
        };

        // NOTE: Raw audio is NOT sent to recording saver to prevent echo
        // Only the mixed audio (from AudioPipeline) is saved to file (see pipeline.rs:726-736)
        // This ensures we only record once: mic + system properly mixed
        // Individual raw streams go only to the transcription pipeline below

        // Send to processing pipeline for transcription
        if let Err(e) = self.state.send_audio_chunk(audio_chunk) {
            // Check if this is the "pipeline not ready" error
            if e.to_string().contains("Audio pipeline not ready") {
                // This is expected during initialization, just log it as debug
                debug!("Audio pipeline not ready yet, skipping chunk {}", chunk_id);
                return;
            }

            warn!("Failed to send audio chunk: {}", e);
            // More specific error handling based on failure reason
            let error = if e.to_string().contains("channel closed") {
                AudioError::ChannelClosed
            } else if e.to_string().contains("full") {
                AudioError::BufferOverflow
            } else {
                AudioError::ProcessingFailed
            };
            self.state.report_error(error);
        } else {
            debug!("Sent audio chunk {} ({} samples)", chunk_id, data.len());
        }
    }

    /// Handle stream errors with enhanced disconnect detection
    pub fn handle_stream_error(&self, error: cpal::StreamError) {
        error!("Audio stream error for {}: {}", self.device.name, error);

        let error_str = error.to_string().to_lowercase();

        // Enhanced error detection for device disconnection
        let audio_error = if error_str.contains("device is no longer available")
            || error_str.contains("device not found")
            || error_str.contains("device disconnected")
            || error_str.contains("no such device")
            || error_str.contains("device unavailable")
            || error_str.contains("device removed")
        {
            warn!("🔌 Device disconnect detected for: {}", self.device.name);
            AudioError::DeviceDisconnected
        } else if error_str.contains("permission") || error_str.contains("access denied") {
            AudioError::PermissionDenied
        } else if error_str.contains("channel closed") {
            AudioError::ChannelClosed
        } else if error_str.contains("stream") && error_str.contains("failed") {
            AudioError::StreamFailed
        } else {
            warn!("Unknown audio error: {}", error);
            AudioError::StreamFailed
        };

        self.state.report_error(audio_error);
    }
}

const STREAMING_ASR_DROP_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

fn streaming_audio_source(capture_topology: CaptureTopology) -> AudioSource {
    match (capture_topology.microphone, capture_topology.system_audio) {
        (true, false) => AudioSource::Microphone,
        (false, true) => AudioSource::System,
        _ => AudioSource::Mixed,
    }
}

/// Non-blocking ingress from the canonical recording clock to a streaming ASR actor.
///
/// The bounded provider queue is deliberately isolated from recording and VAD.
/// When it cannot keep up, only the provider frame is dropped; the canonical
/// recording path continues without waiting.
struct StreamingAsrIngress {
    sender: Option<mpsc::Sender<StreamingAsrCommand>>,
    next_sequence: u64,
    dropped_canonical_frames: u64,
    last_drop_log: Option<std::time::Instant>,
}

impl StreamingAsrIngress {
    fn new(sender: Option<mpsc::Sender<StreamingAsrCommand>>) -> Self {
        Self {
            sender,
            next_sequence: 0,
            dropped_canonical_frames: 0,
            last_drop_log: None,
        }
    }

    fn send_window(&mut self, window: &AlignedAudioWindow, source: AudioSource) {
        if self.sender.is_none() {
            return;
        }

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let frame =
            StreamingAudioFrame::new(sequence, window.start_frame, window.mixed.clone(), source);
        let frame_count = frame.samples.len() as u64;
        self.try_send(StreamingAsrCommand::Audio(frame), frame_count, "audio");
    }

    fn commit(&mut self, through_frame: u64) {
        if self.sender.is_none() {
            return;
        }
        self.try_send(
            StreamingAsrCommand::Commit { through_frame },
            0,
            "speech endpoint commit",
        );
    }

    fn finish(&mut self) {
        if self.sender.is_some() {
            self.try_send(
                StreamingAsrCommand::Flush {
                    reason: StreamingFlushReason::RecordingStopped,
                },
                0,
                "recording-stopped flush",
            );
            self.try_send(StreamingAsrCommand::Stop, 0, "stop");
        }

        // This ingress is session-scoped. Dropping its final sender clone also
        // guarantees receiver EOF when a saturated queue rejected Stop.
        self.sender = None;
        if self.dropped_canonical_frames > 0 {
            warn!(
                "Streaming ASR session ended after dropping {} canonical 48 kHz frames ({:.1} ms); recording and local VAD were unaffected",
                self.dropped_canonical_frames,
                self.dropped_canonical_frames as f64 / 48.0
            );
        }
    }

    fn try_send(&mut self, command: StreamingAsrCommand, frame_count: u64, label: &str) {
        let result = match self.sender.as_ref() {
            Some(sender) => sender.try_send(command),
            None => return,
        };

        match result {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                if frame_count > 0 {
                    self.dropped_canonical_frames =
                        self.dropped_canonical_frames.saturating_add(frame_count);
                    let should_log = self.last_drop_log.map_or(true, |last_log| {
                        last_log.elapsed() >= STREAMING_ASR_DROP_LOG_INTERVAL
                    });
                    if should_log {
                        warn!(
                            "Streaming ASR queue is full; dropped {} canonical 48 kHz frames total ({:.1} ms) without blocking recording",
                            self.dropped_canonical_frames,
                            self.dropped_canonical_frames as f64 / 48.0
                        );
                        self.last_drop_log = Some(std::time::Instant::now());
                    }
                } else {
                    warn!(
                        "Streaming ASR queue is full; lifecycle command '{}' was not queued",
                        label
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "Streaming ASR receiver closed while sending '{}'; disabling cloud ingress for this recording",
                    label
                );
                self.sender = None;
            }
        }
    }
}

/// VAD-driven audio processing pipeline
/// Uses Voice Activity Detection to segment speech in real-time and send only speech to Whisper
pub struct AudioPipeline {
    receiver: mpsc::UnboundedReceiver<AudioChunk>,
    transcription_sender: mpsc::UnboundedSender<AudioChunk>,
    state: Arc<RecordingState>,
    vad_processor: ContinuousVadProcessor,
    sample_rate: u32,
    chunk_id_counter: u64,
    // Performance optimization: reduce logging frequency
    last_summary_time: std::time::Instant,
    processed_chunks: u64,
    // Smart batching for audio metrics
    metrics_batcher: Option<AudioMetricsBatcher>,
    // Shared 48 kHz media timeline. Capture callback boundaries may differ;
    // the synchronizer produces frame-aligned 50 ms windows and explicit gaps.
    synchronizer: AudioSynchronizer,
    gap_health: GapHealthTracker,
    capture_topology: CaptureTopology,
    // Recording sender receives aligned microphone/system/mixed windows.
    recording_sender_for_mixed: Option<mpsc::UnboundedSender<AudioChunk>>,
    streaming_asr_ingress: StreamingAsrIngress,
}

impl AudioPipeline {
    pub fn new(
        receiver: mpsc::UnboundedReceiver<AudioChunk>,
        transcription_sender: mpsc::UnboundedSender<AudioChunk>,
        state: Arc<RecordingState>,
        target_chunk_duration_ms: u32,
        sample_rate: u32,
        mic_device_name: String,
        mic_device_kind: super::device_detection::InputDeviceKind,
        system_device_name: String,
        system_device_kind: super::device_detection::InputDeviceKind,
        capture_topology: CaptureTopology,
        streaming_asr_sender: Option<mpsc::Sender<StreamingAsrCommand>>,
    ) -> Result<Self> {
        // Log device characteristics for adaptive buffering
        info!("🎛️ AudioPipeline initializing with device characteristics:");
        info!(
            "   Mic: '{}' ({:?}) - Buffer: {:?}",
            mic_device_name,
            mic_device_kind,
            mic_device_kind.buffer_timeout()
        );
        info!(
            "   System: '{}' ({:?}) - Buffer: {:?}",
            system_device_name,
            system_device_kind,
            system_device_kind.buffer_timeout()
        );

        // Device kind information can be used for adaptive buffering in the future
        // For now, we log it for monitoring and potential optimization
        let _ = (
            mic_device_name,
            mic_device_kind,
            system_device_name,
            system_device_kind,
            capture_topology,
        );

        // Live captions should react to natural pauses quickly, but video speech
        // can continue for tens of seconds without one. Bound active speech to
        // short bounded chunks so transcription can start while the speaker is
        // still talking. Batch/imported-audio VAD remains unbounded.
        let redemption_time = 400;
        let max_live_segment_duration_ms = 2_500;

        let vad_processor = ContinuousVadProcessor::new_with_max_segment_duration(
            sample_rate,
            redemption_time,
            Some(max_live_segment_duration_ms),
        )
        .map_err(|error| anyhow::anyhow!("Failed to create VAD processor: {error}"))?;
        info!(
            "VAD-driven pipeline: natural pauses or {}ms maximum live chunks are sent to transcription",
            max_live_segment_duration_ms
        );

        let synchronizer = AudioSynchronizer::new(capture_topology)?;

        // Note: target_chunk_duration_ms is ignored - VAD controls segmentation now
        let _ = target_chunk_duration_ms;

        Ok(Self {
            receiver,
            transcription_sender,
            state,
            vad_processor,
            sample_rate,
            chunk_id_counter: 0,
            // Performance optimization: reduce logging frequency
            last_summary_time: std::time::Instant::now(),
            processed_chunks: 0,
            // Initialize metrics batcher for smart batching
            metrics_batcher: Some(AudioMetricsBatcher::new()),
            synchronizer,
            gap_health: GapHealthTracker::default(),
            capture_topology,
            recording_sender_for_mixed: None, // Will be set by manager
            streaming_asr_ingress: StreamingAsrIngress::new(streaming_asr_sender),
        })
    }

    /// Run the VAD-driven audio processing pipeline
    pub async fn run(mut self) -> Result<()> {
        info!("VAD-driven audio pipeline started - segments sent in real-time based on speech detection");

        // CRITICAL FIX: Continue processing until channel is closed, not based on recording state
        // This ensures ALL chunks are processed during shutdown, fixing premature meeting completion
        // Previous bug: Loop checked `while self.state.is_recording()` which caused early exit when
        // stop_recording() was called, losing flush signals and remaining chunks in the pipeline
        loop {
            // Receive audio chunks with timeout
            match tokio::time::timeout(
                std::time::Duration::from_millis(50), // Shorter timeout for responsiveness
                self.receiver.recv(),
            )
            .await
            {
                Ok(Some(chunk)) => {
                    // PERFORMANCE: Check for flush signal (special chunk with ID >= u64::MAX - 10)
                    // Multiple flush signals may be sent to ensure processing
                    if chunk.chunk_id >= u64::MAX - 10 {
                        info!(
                            "📥 Received FLUSH signal #{} - flushing VAD processor",
                            u64::MAX - chunk.chunk_id
                        );
                        self.flush_synchronizer()?;
                        self.flush_remaining_audio()?;
                        // Continue processing to handle any remaining chunks
                        continue;
                    }

                    // PERFORMANCE OPTIMIZATION: Eliminate per-chunk logging overhead
                    // Logging in hot paths causes severe performance degradation
                    self.processed_chunks += 1;

                    // Smart batching: collect metrics instead of logging every chunk
                    if let Some(ref batcher) = self.metrics_batcher {
                        let avg_level = chunk.data.iter().map(|&x| x.abs()).sum::<f32>()
                            / chunk.data.len() as f32;
                        let duration_ms =
                            chunk.data.len() as f64 / chunk.sample_rate as f64 * 1000.0;

                        batch_audio_metric!(
                            Some(batcher),
                            chunk.chunk_id,
                            chunk.data.len(),
                            duration_ms,
                            avg_level
                        );
                    }

                    // CRITICAL: Log summary only every 200 chunks OR every 60 seconds (99.5% reduction)
                    // This eliminates I/O overhead in the audio processing hot path
                    // Use performance-optimized debug macro that compiles to nothing in release builds
                    if self.processed_chunks % 200 == 0
                        || self.last_summary_time.elapsed().as_secs() >= 60
                    {
                        perf_debug!(
                            "Pipeline processed {} chunks, current chunk: {} ({} samples)",
                            self.processed_chunks,
                            chunk.chunk_id,
                            chunk.data.len()
                        );
                        self.last_summary_time = std::time::Instant::now();
                    }

                    let track = match chunk.device_type {
                        DeviceType::Microphone => AudioTrack::Microphone,
                        DeviceType::System => AudioTrack::SystemAudio,
                        DeviceType::Mixed => {
                            warn!("Ignoring derived mixed chunk at capture input");
                            continue;
                        }
                    };
                    let start_frame = chunk.start_frame.unwrap_or_else(|| {
                        (chunk.timestamp.max(0.0) * self.sample_rate as f64).round() as u64
                    });

                    match self
                        .synchronizer
                        .push_chunk(track, start_frame, &chunk.data)
                    {
                        Ok(windows) => {
                            for window in windows {
                                self.process_aligned_window(window)?;
                            }
                        }
                        Err(error) => warn!("Audio synchronization error: {}", error),
                    }
                }
                Ok(None) => {
                    info!(
                        "Audio pipeline: sender closed after processing {} chunks",
                        self.processed_chunks
                    );
                    break;
                }
                Err(_) => {
                    // Timeout - just continue, VAD handles all segmentation
                    continue;
                }
            }
        }

        let final_drain_result = self
            .flush_synchronizer()
            .and_then(|_| self.flush_remaining_audio());
        self.streaming_asr_ingress.finish();
        final_drain_result?;

        info!("VAD-driven audio pipeline ended");
        Ok(())
    }

    fn process_aligned_window(&mut self, window: AlignedAudioWindow) -> Result<()> {
        for gap in &window.flags.gaps {
            if gap.reason != GapReason::TrackNotCaptured {
                warn!(
                    "Audio gap: track={:?}, frames={}..{}, reason={:?}",
                    gap.track,
                    gap.start_frame,
                    gap.end_frame(),
                    gap.reason
                );
            }
        }

        // Convert audio-rate gap flags into low-volume state transitions. The
        // shared timeline session allocates sequence numbers and persists the
        // exact event; repeated 50 ms gap windows are intentionally collapsed
        // until the source recovers.
        for draft in self.gap_health.observe_window(
            window.start_frame,
            window.end_frame(),
            &window.flags.gaps,
        ) {
            match record_audio_timeline_event(draft) {
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => warn!("Failed to persist synchronized audio gap: {}", error),
            }
        }

        self.state.update_media_end_frame(window.end_frame());

        if let Some(ref sender) = self.recording_sender_for_mixed {
            let timestamp = window.start_frame as f64 / window.sample_rate as f64;
            let send_track = |data: Vec<f32>, device_type: DeviceType| {
                if let Err(error) = sender.send(AudioChunk {
                    data,
                    sample_rate: window.sample_rate,
                    timestamp,
                    chunk_id: window.start_frame,
                    device_type,
                    start_frame: Some(window.start_frame),
                    end_frame: Some(window.end_frame()),
                }) {
                    warn!("Failed to send aligned recording track: {}", error);
                }
            };

            if self.capture_topology.microphone {
                send_track(window.microphone.clone(), DeviceType::Microphone);
            }
            if self.capture_topology.system_audio {
                send_track(window.system_audio.clone(), DeviceType::System);
            }
            send_track(window.mixed.clone(), DeviceType::Mixed);
        }

        // Stream the exact canonical window after it has entered recording,
        // but before VAD segmentation. try_send keeps provider backpressure
        // completely off the recording hot path.
        self.streaming_asr_ingress
            .send_window(&window, streaming_audio_source(self.capture_topology));

        match self.vad_processor.process_audio(&window.mixed) {
            Ok(speech_segments) => {
                let reached_endpoint = !speech_segments.is_empty();
                for segment in speech_segments {
                    self.send_speech_segment(segment.samples, segment.start_timestamp_ms, false);
                }
                if reached_endpoint {
                    let through_frame =
                        window.start_frame.saturating_add(window.mixed.len() as u64);
                    self.streaming_asr_ingress.commit(through_frame);
                }
            }
            Err(error) => warn!("VAD error: {}", error),
        }

        Ok(())
    }

    fn asr_device_type(&self) -> DeviceType {
        match (
            self.capture_topology.microphone,
            self.capture_topology.system_audio,
        ) {
            (true, false) => DeviceType::Microphone,
            (false, true) => DeviceType::System,
            _ => DeviceType::Mixed,
        }
    }

    fn send_speech_segment(&mut self, samples: Vec<f32>, start_ms: f64, final_flush: bool) {
        if samples.len() < 800 {
            debug!(
                "Skipping short{} VAD segment: {} samples < 800",
                if final_flush { " final" } else { "" },
                samples.len()
            );
            return;
        }

        let start_frame = (start_ms.max(0.0) * 48.0).round() as u64;
        let duration_frames = (samples.len() as u64).saturating_mul(3);
        let transcription_chunk = AudioChunk {
            data: samples,
            sample_rate: 16_000,
            timestamp: start_ms / 1_000.0,
            chunk_id: self.chunk_id_counter,
            device_type: self.asr_device_type(),
            start_frame: Some(start_frame),
            end_frame: Some(start_frame.saturating_add(duration_frames)),
        };

        if let Err(error) = self.transcription_sender.send(transcription_chunk) {
            warn!("Failed to send VAD segment: {}", error);
        } else {
            self.chunk_id_counter += 1;
        }
    }

    fn flush_synchronizer(&mut self) -> Result<()> {
        let windows = self.synchronizer.drain_final();
        for window in windows {
            self.process_aligned_window(window)?;
        }
        let final_frame = self.state.get_media_end_frame();
        for draft in self.gap_health.finish_session(final_frame) {
            match record_audio_timeline_event(draft) {
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => warn!("Failed to persist terminal audio gap: {}", error),
            }
        }
        Ok(())
    }

    fn flush_remaining_audio(&mut self) -> Result<()> {
        info!(
            "Flushing remaining audio from pipeline (processed {} chunks)",
            self.processed_chunks
        );

        // Flush any remaining audio from VAD processor and send segments to transcription
        match self.vad_processor.flush() {
            Ok(final_segments) => {
                for segment in final_segments {
                    let duration_ms = segment.end_timestamp_ms - segment.start_timestamp_ms;

                    // Send segments >= 50ms (800 samples at 16kHz) - matches main pipeline filter
                    if segment.samples.len() >= 800 {
                        info!(
                            "📤 Sending final VAD segment to Whisper: {:.1}ms duration, {} samples",
                            duration_ms,
                            segment.samples.len()
                        );

                        self.send_speech_segment(segment.samples, segment.start_timestamp_ms, true);
                    } else {
                        info!(
                            "⏭️ Skipping short final segment: {:.1}ms ({} samples < 800)",
                            duration_ms,
                            segment.samples.len()
                        );
                    }
                }
            }
            Err(e) => {
                warn!("Failed to flush VAD processor: {}", e);
            }
        }

        Ok(())
    }
}

/// Simple audio pipeline manager
pub struct AudioPipelineManager {
    pipeline_handle: Option<JoinHandle<Result<()>>>,
    audio_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
    pending_audio_receiver: Option<mpsc::UnboundedReceiver<AudioChunk>>,
    streaming_asr_sender: Option<mpsc::Sender<StreamingAsrCommand>>,
}

impl AudioPipelineManager {
    pub fn new() -> Self {
        Self {
            pipeline_handle: None,
            audio_sender: None,
            pending_audio_receiver: None,
            streaming_asr_sender: None,
        }
    }

    /// Configure a bounded streaming-ASR ingress for the next recording.
    pub fn set_streaming_asr_sender(&mut self, sender: mpsc::Sender<StreamingAsrCommand>) {
        self.streaming_asr_sender = Some(sender);
    }

    /// Install the capture ingress before opening hardware streams.
    ///
    /// Device creation is intentionally attempted before the synchronizer is
    /// configured so a failed optional route can be removed from the actual
    /// topology. Capture callbacks that arrive during that short setup window
    /// queue here instead of being discarded.
    pub fn prepare_capture_channel(&mut self, state: Arc<RecordingState>) -> Result<()> {
        if self.pipeline_handle.is_some()
            || self.audio_sender.is_some()
            || self.pending_audio_receiver.is_some()
        {
            return Err(anyhow::anyhow!("Audio capture channel is already prepared"));
        }

        let (audio_sender, audio_receiver) = mpsc::unbounded_channel::<AudioChunk>();
        state.set_audio_sender(audio_sender.clone());
        self.audio_sender = Some(audio_sender);
        self.pending_audio_receiver = Some(audio_receiver);
        Ok(())
    }

    /// Start the audio pipeline with device information for adaptive buffering
    pub fn start(
        &mut self,
        state: Arc<RecordingState>,
        transcription_sender: mpsc::UnboundedSender<AudioChunk>,
        target_chunk_duration_ms: u32,
        sample_rate: u32,
        recording_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
        mic_device_name: String,
        mic_device_kind: super::device_detection::InputDeviceKind,
        system_device_name: String,
        system_device_kind: super::device_detection::InputDeviceKind,
        capture_topology: CaptureTopology,
    ) -> Result<()> {
        // Log device information for adaptive buffering
        info!("🎙️ Starting pipeline with device info:");
        info!(
            "   Microphone: '{}' ({:?})",
            mic_device_name, mic_device_kind
        );
        info!(
            "   System Audio: '{}' ({:?})",
            system_device_name, system_device_kind
        );

        if self.pipeline_handle.is_some() {
            return Err(anyhow::anyhow!("Audio pipeline is already running"));
        }

        // Prefer the preinstalled channel, which may already contain the first
        // hardware callbacks. Legacy callers can still start directly.
        let audio_receiver = if let Some(receiver) = self.pending_audio_receiver.take() {
            receiver
        } else {
            let (audio_sender, audio_receiver) = mpsc::unbounded_channel::<AudioChunk>();
            state.set_audio_sender(audio_sender.clone());
            self.audio_sender = Some(audio_sender);
            audio_receiver
        };

        // Create and start pipeline with device information for adaptive mixing
        let mut pipeline = AudioPipeline::new(
            audio_receiver,
            transcription_sender,
            state.clone(),
            target_chunk_duration_ms,
            sample_rate,
            mic_device_name,
            mic_device_kind,
            system_device_name,
            system_device_kind,
            capture_topology,
            self.streaming_asr_sender.clone(),
        )?;

        // CRITICAL FIX: Connect recording sender to receive pre-mixed audio
        // This ensures both mic AND system audio are captured in recordings
        pipeline.recording_sender_for_mixed = recording_sender;

        let handle = tokio::spawn(async move { pipeline.run().await });

        // A provider actor is scoped to one recording. A new recording must
        // explicitly install its own sender rather than reuse a closed actor.
        self.streaming_asr_sender = None;
        self.pipeline_handle = Some(handle);
        info!("Audio pipeline manager started with mixed audio recording");
        Ok(())
    }

    /// Stop the audio pipeline
    pub async fn stop(&mut self) -> Result<()> {
        // Drop the sender to close the pipeline
        self.audio_sender = None;
        self.pending_audio_receiver = None;
        self.streaming_asr_sender = None;

        // Wait for pipeline to finish
        if let Some(handle) = self.pipeline_handle.take() {
            match handle.await {
                Ok(result) => result,
                Err(e) => {
                    error!("Pipeline task failed: {}", e);
                    Err(anyhow::anyhow!("Audio pipeline task failed: {e}"))
                }
            }
        } else {
            Ok(())
        }
    }

    /// Force immediate flush of accumulated audio and stop pipeline
    /// PERFORMANCE CRITICAL: Eliminates 30+ second shutdown delays
    pub async fn force_flush_and_stop(&mut self) -> Result<()> {
        info!("🚀 Force flushing pipeline - processing ALL accumulated audio immediately");

        // If we have a sender, send a special flush signal first
        if let Some(sender) = &self.audio_sender {
            // Create a special flush chunk to trigger immediate processing
            let flush_chunk = AudioChunk {
                data: vec![], // Empty data signals flush
                sample_rate: 16000,
                timestamp: 0.0,
                chunk_id: u64::MAX, // Special ID to indicate flush
                device_type: super::recording_state::DeviceType::Microphone,
                start_frame: None,
                end_frame: None,
            };

            if let Err(e) = sender.send(flush_chunk) {
                warn!("Failed to send flush signal: {}", e);
            } else {
                info!("📤 Sent flush signal to pipeline");

                // PERFORMANCE OPTIMIZATION: Reduced wait time from 50ms to 20ms
                // Pipeline should process flush signal very quickly
                tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

                // Send multiple flush signals to ensure the pipeline catches it
                // This aggressive approach eliminates shutdown delay issues
                for i in 0..3 {
                    let additional_flush = AudioChunk {
                        data: vec![],
                        sample_rate: 16000,
                        timestamp: 0.0,
                        chunk_id: u64::MAX - (i as u64),
                        device_type: super::recording_state::DeviceType::Microphone,
                        start_frame: None,
                        end_frame: None,
                    };
                    let _ = sender.send(additional_flush);
                }

                info!("📤 Sent additional flush signals for reliability");
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        }

        // Now stop normally
        self.stop().await
    }
}

impl Default for AudioPipelineManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod streaming_ingress_tests {
    use super::*;
    use crate::audio::synchronizer::WindowFlags;

    fn aligned_window(start_frame: u64, value: f32) -> AlignedAudioWindow {
        AlignedAudioWindow {
            sample_rate: 48_000,
            start_frame,
            frame_count: 2_400,
            microphone: vec![value; 2_400],
            system_audio: vec![value; 2_400],
            mixed: vec![value; 2_400],
            flags: WindowFlags::default(),
        }
    }

    #[test]
    fn capture_topology_maps_to_the_actual_streaming_audio_source() {
        assert_eq!(
            streaming_audio_source(CaptureTopology::microphone_only()),
            AudioSource::Microphone
        );
        assert_eq!(
            streaming_audio_source(CaptureTopology::system_audio_only()),
            AudioSource::System
        );
        assert_eq!(
            streaming_audio_source(CaptureTopology::dual()),
            AudioSource::Mixed
        );
    }

    #[test]
    fn bounded_queue_full_drops_only_streaming_frames_without_blocking() {
        let (sender, mut receiver) = mpsc::channel(1);
        let mut ingress = StreamingAsrIngress::new(Some(sender));

        ingress.send_window(&aligned_window(0, 0.1), AudioSource::Mixed);
        ingress.send_window(&aligned_window(2_400, 0.2), AudioSource::Mixed);

        assert_eq!(ingress.dropped_canonical_frames, 2_400);
        assert_eq!(ingress.next_sequence, 2);
        match receiver.try_recv().unwrap() {
            StreamingAsrCommand::Audio(frame) => {
                assert_eq!(frame.sequence, 0);
                assert_eq!(frame.origin_frame, 0);
                assert_eq!(frame.samples.len(), 2_400);
            }
            command => panic!("expected audio frame, received {command:?}"),
        }

        // Once capacity is available again, the sequence exposes the dropped
        // canonical window instead of silently renumbering provider input.
        ingress.send_window(&aligned_window(4_800, 0.3), AudioSource::Mixed);
        match receiver.try_recv().unwrap() {
            StreamingAsrCommand::Audio(frame) => {
                assert_eq!(frame.sequence, 2);
                assert_eq!(frame.origin_frame, 4_800);
            }
            command => panic!("expected audio frame, received {command:?}"),
        }
    }

    #[test]
    fn closed_receiver_disables_streaming_ingress() {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let mut ingress = StreamingAsrIngress::new(Some(sender));

        ingress.send_window(&aligned_window(0, 0.1), AudioSource::System);

        assert!(ingress.sender.is_none());
        assert_eq!(ingress.dropped_canonical_frames, 0);
    }

    #[test]
    fn final_lifecycle_commands_are_flush_then_stop() {
        let (sender, mut receiver) = mpsc::channel(2);
        let mut ingress = StreamingAsrIngress::new(Some(sender));

        ingress.finish();

        assert!(matches!(
            receiver.try_recv().unwrap(),
            StreamingAsrCommand::Flush {
                reason: StreamingFlushReason::RecordingStopped
            }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            StreamingAsrCommand::Stop
        ));
        assert!(ingress.sender.is_none());
    }

    #[test]
    fn speech_endpoint_commit_follows_the_canonical_audio_window() {
        let (sender, mut receiver) = mpsc::channel(2);
        let mut ingress = StreamingAsrIngress::new(Some(sender));

        ingress.send_window(&aligned_window(4_800, 0.2), AudioSource::System);
        ingress.commit(7_200);

        assert!(matches!(
            receiver.try_recv().unwrap(),
            StreamingAsrCommand::Audio(frame)
                if frame.origin_frame == 4_800 && frame.source == AudioSource::System
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            StreamingAsrCommand::Commit {
                through_frame: 7_200
            }
        ));
    }
}
