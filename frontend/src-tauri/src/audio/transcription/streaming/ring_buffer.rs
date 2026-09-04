//! Bounded canonical-audio history used to replay a short overlap after a
//! provider connection failure.

use super::protocol::{StreamingAudioFrame, StreamingProtocolError, STREAMING_AUDIO_SAMPLE_RATE};
use std::collections::VecDeque;
use thiserror::Error;

pub const DEFAULT_RING_SECONDS: u64 = 30;
pub const DEFAULT_REPLAY_PREROLL_MS: u64 = 1_500;
pub const DEFAULT_RING_FRAMES: u64 = DEFAULT_RING_SECONDS * STREAMING_AUDIO_SAMPLE_RATE as u64;
pub const DEFAULT_REPLAY_PREROLL_FRAMES: u64 =
    DEFAULT_REPLAY_PREROLL_MS * STREAMING_AUDIO_SAMPLE_RATE as u64 / 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingWriteReport {
    pub received_frames: u64,
    /// Total frames evicted by this write, including an oversized incoming
    /// frame's prefix when that prefix can never fit in the ring.
    pub evicted_frames: u64,
    pub incoming_prefix_discarded: u64,
    pub oldest_frame: u64,
    pub newest_frame_exclusive: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayAudioWindow {
    pub stable_boundary_frame: u64,
    pub requested_start_frame: u64,
    pub actual_start_frame: u64,
    pub end_frame_exclusive: u64,
    pub history_truncated: bool,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct StreamingAudioRingBuffer {
    capacity_frames: usize,
    samples: VecDeque<f32>,
    oldest_frame: Option<u64>,
    newest_frame_exclusive: Option<u64>,
    last_sequence: Option<u64>,
}

impl StreamingAudioRingBuffer {
    pub fn thirty_seconds() -> Self {
        Self::with_capacity_frames(DEFAULT_RING_FRAMES)
            .expect("the fixed 30 second ring capacity must fit in usize")
    }

    pub fn with_capacity_frames(capacity_frames: u64) -> Result<Self, RingBufferError> {
        if capacity_frames == 0 {
            return Err(RingBufferError::ZeroCapacity);
        }
        let capacity_frames =
            usize::try_from(capacity_frames).map_err(|_| RingBufferError::CapacityTooLarge)?;
        Ok(Self {
            capacity_frames,
            samples: VecDeque::with_capacity(capacity_frames),
            oldest_frame: None,
            newest_frame_exclusive: None,
            last_sequence: None,
        })
    }

    pub fn capacity_frames(&self) -> u64 {
        self.capacity_frames as u64
    }

    pub fn len_frames(&self) -> u64 {
        self.samples.len() as u64
    }

    pub fn oldest_frame(&self) -> Option<u64> {
        self.oldest_frame
    }

    pub fn newest_frame_exclusive(&self) -> Option<u64> {
        self.newest_frame_exclusive
    }

    /// Append a clock-contiguous window. Sequence numbers must be strictly
    /// increasing and canonical frame ranges must be exactly contiguous. The
    /// synchronizer is responsible for representing known gaps as silence.
    pub fn push(
        &mut self,
        frame: &StreamingAudioFrame,
    ) -> Result<RingWriteReport, RingBufferError> {
        frame.validate()?;
        if let Some(last_sequence) = self.last_sequence {
            if frame.sequence <= last_sequence {
                return Err(RingBufferError::NonMonotonicSequence {
                    last_sequence,
                    received_sequence: frame.sequence,
                });
            }
        }
        if let Some(expected_frame) = self.newest_frame_exclusive {
            if frame.origin_frame != expected_frame {
                return Err(RingBufferError::NonContiguousTimeline {
                    expected_frame,
                    received_frame: frame.origin_frame,
                });
            }
        }

        let frame_end = frame.end_frame()?;
        let received_frames =
            u64::try_from(frame.samples.len()).map_err(|_| RingBufferError::FrameIndexOverflow)?;
        let mut incoming_prefix_discarded = 0_u64;
        let evicted_frames;

        if frame.samples.len() >= self.capacity_frames {
            incoming_prefix_discarded = u64::try_from(frame.samples.len() - self.capacity_frames)
                .map_err(|_| RingBufferError::FrameIndexOverflow)?;
            evicted_frames = self
                .len_frames()
                .checked_add(incoming_prefix_discarded)
                .ok_or(RingBufferError::FrameIndexOverflow)?;
            self.samples.clear();
            self.samples.extend(
                frame.samples[frame.samples.len() - self.capacity_frames..]
                    .iter()
                    .copied(),
            );
            self.oldest_frame = Some(
                frame_end
                    .checked_sub(self.capacity_frames as u64)
                    .ok_or(RingBufferError::FrameIndexOverflow)?,
            );
        } else {
            let overflow = self
                .samples
                .len()
                .saturating_add(frame.samples.len())
                .saturating_sub(self.capacity_frames);
            evicted_frames = overflow as u64;
            if overflow > 0 {
                self.samples.drain(..overflow);
            }
            self.samples.extend(frame.samples.iter().copied());
            self.oldest_frame = Some(match self.oldest_frame {
                Some(oldest) => oldest
                    .checked_add(evicted_frames)
                    .ok_or(RingBufferError::FrameIndexOverflow)?,
                None => frame.origin_frame,
            });
        }

        self.newest_frame_exclusive = Some(frame_end);
        self.last_sequence = Some(frame.sequence);
        let oldest_frame = self.oldest_frame.expect("a non-empty frame was appended");
        Ok(RingWriteReport {
            received_frames,
            evicted_frames,
            incoming_prefix_discarded,
            oldest_frame,
            newest_frame_exclusive: frame_end,
        })
    }

    /// Select replay audio beginning 1.5 seconds before the last stable
    /// transcript boundary. If the 30 second ring no longer contains the full
    /// requested overlap, `history_truncated` makes the loss explicit.
    pub fn replay_from_stable_boundary(
        &self,
        stable_boundary_frame: u64,
    ) -> Result<Option<ReplayAudioWindow>, RingBufferError> {
        let (Some(oldest), Some(newest)) = (self.oldest_frame, self.newest_frame_exclusive) else {
            return Ok(None);
        };
        if stable_boundary_frame > newest {
            return Err(RingBufferError::StableBoundaryAfterAudio {
                stable_boundary_frame,
                newest_frame_exclusive: newest,
            });
        }

        let requested_start = stable_boundary_frame.saturating_sub(DEFAULT_REPLAY_PREROLL_FRAMES);
        let actual_start = requested_start.max(oldest);
        let offset = usize::try_from(actual_start - oldest)
            .map_err(|_| RingBufferError::FrameIndexOverflow)?;
        let samples = self.samples.iter().skip(offset).copied().collect();
        Ok(Some(ReplayAudioWindow {
            stable_boundary_frame,
            requested_start_frame: requested_start,
            actual_start_frame: actual_start,
            end_frame_exclusive: newest,
            history_truncated: actual_start > requested_start,
            samples,
        }))
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.oldest_frame = None;
        self.newest_frame_exclusive = None;
        self.last_sequence = None;
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RingBufferError {
    #[error("streaming ASR ring capacity cannot be zero")]
    ZeroCapacity,
    #[error("streaming ASR ring capacity does not fit this platform")]
    CapacityTooLarge,
    #[error("audio frame sequence {received_sequence} is not greater than {last_sequence}")]
    NonMonotonicSequence {
        last_sequence: u64,
        received_sequence: u64,
    },
    #[error("audio frame begins at {received_frame}, expected contiguous frame {expected_frame}")]
    NonContiguousTimeline {
        expected_frame: u64,
        received_frame: u64,
    },
    #[error(
        "stable boundary {stable_boundary_frame} is after buffered audio end {newest_frame_exclusive}"
    )]
    StableBoundaryAfterAudio {
        stable_boundary_frame: u64,
        newest_frame_exclusive: u64,
    },
    #[error("streaming ring frame index overflow")]
    FrameIndexOverflow,
    #[error(transparent)]
    InvalidFrame(#[from] StreamingProtocolError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcription::AudioSource;

    fn frame(sequence: u64, origin: u64, values: &[f32]) -> StreamingAudioFrame {
        StreamingAudioFrame::new(sequence, origin, values.to_vec(), AudioSource::Mixed)
    }

    #[test]
    fn ring_reports_eviction_and_keeps_exact_canonical_range() {
        let mut ring = StreamingAudioRingBuffer::with_capacity_frames(5).unwrap();
        assert_eq!(
            ring.push(&frame(0, 10, &[1.0, 2.0, 3.0]))
                .unwrap()
                .evicted_frames,
            0
        );
        let report = ring.push(&frame(2, 13, &[4.0, 5.0, 6.0, 7.0])).unwrap();

        assert_eq!(report.evicted_frames, 2);
        assert_eq!(report.oldest_frame, 12);
        assert_eq!(report.newest_frame_exclusive, 17);
        let replay = ring.replay_from_stable_boundary(17).unwrap().unwrap();
        assert_eq!(replay.samples, vec![3.0, 4.0, 5.0, 6.0, 7.0]);
        assert!(replay.history_truncated);
    }

    #[test]
    fn oversized_frame_drops_only_the_unrecoverable_prefix() {
        let mut ring = StreamingAudioRingBuffer::with_capacity_frames(4).unwrap();
        ring.push(&frame(0, 0, &[9.0, 9.0])).unwrap();
        let report = ring
            .push(&frame(1, 2, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
            .unwrap();
        assert_eq!(report.incoming_prefix_discarded, 2);
        assert_eq!(report.evicted_frames, 4);
        assert_eq!(report.oldest_frame, 4);
        assert_eq!(
            ring.replay_from_stable_boundary(8)
                .unwrap()
                .unwrap()
                .samples,
            vec![3.0, 4.0, 5.0, 6.0]
        );
    }

    #[test]
    fn sequence_and_timeline_are_strict_without_mutating_on_rejection() {
        let mut ring = StreamingAudioRingBuffer::with_capacity_frames(10).unwrap();
        ring.push(&frame(5, 100, &[1.0, 2.0])).unwrap();
        assert!(matches!(
            ring.push(&frame(5, 102, &[3.0])),
            Err(RingBufferError::NonMonotonicSequence { .. })
        ));
        assert!(matches!(
            ring.push(&frame(6, 103, &[3.0])),
            Err(RingBufferError::NonContiguousTimeline { .. })
        ));
        assert_eq!(ring.len_frames(), 2);
        assert_eq!(ring.newest_frame_exclusive(), Some(102));
    }

    #[test]
    fn replay_starts_at_stable_boundary_minus_one_point_five_seconds() {
        let start = 1_000_000;
        let length = DEFAULT_REPLAY_PREROLL_FRAMES as usize + 4_800;
        let samples: Vec<f32> = (0..length).map(|value| value as f32).collect();
        let mut ring = StreamingAudioRingBuffer::with_capacity_frames(length as u64).unwrap();
        ring.push(&frame(0, start, &samples)).unwrap();

        let stable = start + DEFAULT_REPLAY_PREROLL_FRAMES + 2_400;
        let replay = ring.replay_from_stable_boundary(stable).unwrap().unwrap();
        assert_eq!(replay.requested_start_frame, start + 2_400);
        assert_eq!(replay.actual_start_frame, start + 2_400);
        assert!(!replay.history_truncated);
        assert_eq!(replay.samples.first().copied(), Some(2_400.0));
    }

    #[test]
    fn replay_rejects_a_future_stable_boundary() {
        let mut ring = StreamingAudioRingBuffer::with_capacity_frames(10).unwrap();
        ring.push(&frame(0, 20, &[0.0, 0.0])).unwrap();
        assert_eq!(
            ring.replay_from_stable_boundary(23),
            Err(RingBufferError::StableBoundaryAfterAudio {
                stable_boundary_frame: 23,
                newest_frame_exclusive: 22,
            })
        );
    }
}
