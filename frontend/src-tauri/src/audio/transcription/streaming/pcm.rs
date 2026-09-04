//! Stateful conversion from Meetily's 48 kHz mono float clock to provider PCM.
//!
//! Both supported rates are integer divisors of 48 kHz. A small boxcar
//! low-pass/decimator is sufficient for the transport boundary and, unlike a
//! per-frame conversion, carries its phase and partial group across arbitrary
//! 50 ms window splits.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderSampleRate {
    Hz16000,
    Hz24000,
}

impl ProviderSampleRate {
    pub fn hz(self) -> u32 {
        match self {
            Self::Hz16000 => 16_000,
            Self::Hz24000 => 24_000,
        }
    }

    fn decimation_factor(self) -> u8 {
        match self {
            Self::Hz16000 => 3,
            Self::Hz24000 => 2,
        }
    }
}

/// Persistent exact-ratio downsampler for streaming provider audio.
#[derive(Debug, Clone)]
pub struct PersistentPcmResampler {
    target_rate: ProviderSampleRate,
    pending_sum: f64,
    pending_samples: u8,
    consumed_samples: u64,
    produced_samples: u64,
}

impl PersistentPcmResampler {
    pub fn new(target_rate: ProviderSampleRate) -> Self {
        Self {
            target_rate,
            pending_sum: 0.0,
            pending_samples: 0,
            consumed_samples: 0,
            produced_samples: 0,
        }
    }

    pub fn target_rate(&self) -> u32 {
        self.target_rate.hz()
    }

    pub fn consumed_samples(&self) -> u64 {
        self.consumed_samples
    }

    pub fn produced_samples(&self) -> u64 {
        self.produced_samples
    }

    /// Convert one arbitrary-sized 48 kHz frame while retaining fractional
    /// state for the next call.
    pub fn process_f32(&mut self, input: &[f32]) -> Result<Vec<f32>, PcmError> {
        let input_len = u64::try_from(input.len()).map_err(|_| PcmError::CounterOverflow)?;
        self.consumed_samples = self
            .consumed_samples
            .checked_add(input_len)
            .ok_or(PcmError::CounterOverflow)?;

        let factor = self.target_rate.decimation_factor();
        let mut output = Vec::with_capacity(
            (input.len() + usize::from(self.pending_samples)) / usize::from(factor),
        );
        for &sample in input {
            let finite_sample = if sample.is_finite() { sample } else { 0.0 };
            self.pending_sum += f64::from(finite_sample);
            self.pending_samples += 1;
            if self.pending_samples == factor {
                output.push((self.pending_sum / f64::from(factor)) as f32);
                self.pending_sum = 0.0;
                self.pending_samples = 0;
            }
        }

        let output_len = u64::try_from(output.len()).map_err(|_| PcmError::CounterOverflow)?;
        self.produced_samples = self
            .produced_samples
            .checked_add(output_len)
            .ok_or(PcmError::CounterOverflow)?;
        Ok(output)
    }

    pub fn process_pcm16_le(&mut self, input: &[f32]) -> Result<Vec<u8>, PcmError> {
        Ok(pcm16_le_bytes(&self.process_f32(input)?))
    }

    /// Emit a final incomplete decimation group. This is only for an explicit
    /// transport flush/stop; ordinary audio frames must use `process_f32` so
    /// frame boundaries do not reset the phase.
    pub fn finish_f32(&mut self) -> Result<Vec<f32>, PcmError> {
        if self.pending_samples == 0 {
            return Ok(Vec::new());
        }
        let sample = (self.pending_sum / f64::from(self.pending_samples)) as f32;
        self.pending_sum = 0.0;
        self.pending_samples = 0;
        self.produced_samples = self
            .produced_samples
            .checked_add(1)
            .ok_or(PcmError::CounterOverflow)?;
        Ok(vec![sample])
    }

    pub fn finish_pcm16_le(&mut self) -> Result<Vec<u8>, PcmError> {
        Ok(pcm16_le_bytes(&self.finish_f32()?))
    }

    pub fn reset(&mut self) {
        self.pending_sum = 0.0;
        self.pending_samples = 0;
        self.consumed_samples = 0;
        self.produced_samples = 0;
    }
}

/// Convert normalized float samples to signed PCM16 little endian. Scaling by
/// 32768 and clamping afterwards uses both endpoints: -1.0 -> -32768 and
/// +1.0 -> +32767. Non-finite samples become silence.
pub fn pcm16_le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut output = Vec::with_capacity(samples.len().saturating_mul(2));
    for &sample in samples {
        output.extend_from_slice(&f32_to_pcm16(sample).to_le_bytes());
    }
    output
}

pub fn f32_to_pcm16(sample: f32) -> i16 {
    if sample.is_nan() {
        return 0;
    }
    if sample >= 1.0 {
        return i16::MAX;
    }
    if sample <= -1.0 {
        return i16::MIN;
    }

    let scaled = (sample * 32_768.0).round();
    scaled.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PcmError {
    #[error("streaming PCM sample counter overflow")]
    CounterOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcription::streaming::protocol::STREAMING_AUDIO_SAMPLE_RATE;

    #[test]
    fn downsampling_is_identical_across_frame_boundaries() {
        let input = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut one_frame = PersistentPcmResampler::new(ProviderSampleRate::Hz16000);
        let expected = one_frame.process_f32(&input).unwrap();

        let mut split = PersistentPcmResampler::new(ProviderSampleRate::Hz16000);
        let mut actual = split.process_f32(&input[..2]).unwrap();
        actual.extend(split.process_f32(&input[2..5]).unwrap());
        actual.extend(split.process_f32(&input[5..]).unwrap());

        assert_eq!(expected, vec![2.0, 5.0]);
        assert_eq!(actual, expected);
        assert_eq!(split.consumed_samples(), 6);
        assert_eq!(split.produced_samples(), 2);
    }

    #[test]
    fn both_provider_rates_keep_exact_long_run_counts() {
        let input = vec![0.25; STREAMING_AUDIO_SAMPLE_RATE as usize];
        let mut sixteen = PersistentPcmResampler::new(ProviderSampleRate::Hz16000);
        let mut twenty_four = PersistentPcmResampler::new(ProviderSampleRate::Hz24000);

        assert_eq!(sixteen.process_f32(&input).unwrap().len(), 16_000);
        assert_eq!(twenty_four.process_f32(&input).unwrap().len(), 24_000);
    }

    #[test]
    fn explicit_finish_preserves_an_incomplete_tail_once() {
        let mut resampler = PersistentPcmResampler::new(ProviderSampleRate::Hz16000);
        assert!(resampler.process_f32(&[0.25, 0.75]).unwrap().is_empty());
        assert_eq!(resampler.finish_f32().unwrap(), vec![0.5]);
        assert!(resampler.finish_f32().unwrap().is_empty());
    }

    #[test]
    fn pcm16_little_endian_uses_correct_saturation_and_silence_for_nan() {
        let bytes = pcm16_le_bytes(&[
            f32::NEG_INFINITY,
            -1.0,
            -0.5,
            0.0,
            0.5,
            1.0,
            f32::INFINITY,
            f32::NAN,
        ]);
        let decoded: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        assert_eq!(
            decoded,
            vec![
                i16::MIN,
                i16::MIN,
                -16_384,
                0,
                16_384,
                i16::MAX,
                i16::MAX,
                0,
            ]
        );
    }
}
