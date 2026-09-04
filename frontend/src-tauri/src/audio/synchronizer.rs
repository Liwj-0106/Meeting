//! Frame-aligned microphone/system-audio synchronization.
//!
//! Capture callbacks do not necessarily use the same chunk boundaries. This
//! module puts both inputs on one 48 kHz frame clock and emits deterministic
//! 50 ms windows. In dual-track mode a window waits for both sources unless
//! one source has buffered more than the configured skew budget. Once a
//! window is committed, data arriving for an older frame is discarded rather
//! than being allowed to rewrite the timeline.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_WINDOW_MS: u32 = 50;
pub const DEFAULT_MAX_SKEW_MS: u32 = 150;
pub const DEFAULT_WINDOW_FRAMES: usize =
    (DEFAULT_SAMPLE_RATE as usize * DEFAULT_WINDOW_MS as usize) / 1_000;
pub const DEFAULT_MAX_SKEW_FRAMES: usize =
    (DEFAULT_SAMPLE_RATE as usize * DEFAULT_MAX_SKEW_MS as usize) / 1_000;

/// Which capture sources participate in the synchronized timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTopology {
    pub microphone: bool,
    pub system_audio: bool,
}

impl CaptureTopology {
    pub const fn new(microphone: bool, system_audio: bool) -> Self {
        Self {
            microphone,
            system_audio,
        }
    }

    pub const fn microphone_only() -> Self {
        Self::new(true, false)
    }

    pub const fn system_audio_only() -> Self {
        Self::new(false, true)
    }

    pub const fn dual() -> Self {
        Self::new(true, true)
    }

    pub const fn is_active(self, track: AudioTrack) -> bool {
        match track {
            AudioTrack::Microphone => self.microphone,
            AudioTrack::SystemAudio => self.system_audio,
            AudioTrack::Mixed => self.microphone || self.system_audio,
        }
    }

    const fn active_track_count(self) -> usize {
        self.microphone as usize + self.system_audio as usize
    }
}

/// Logical track names used at the synchronizer boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioTrack {
    Microphone,
    SystemAudio,
    Mixed,
}

/// Why samples had to be represented as silence in an aligned window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// This source was not selected in the capture topology.
    TrackNotCaptured,
    /// The other source exceeded the skew budget, so waiting had to stop.
    MaxSkewExceeded,
    /// A later chunk proved that the source skipped this frame range.
    InputDiscontinuity,
    /// Recording ended before this source supplied the remaining frames.
    FinalDrain,
}

/// A half-open missing-frame range: `[start_frame, start_frame + frame_count)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub track: AudioTrack,
    pub start_frame: u64,
    pub frame_count: usize,
    pub reason: GapReason,
}

impl Gap {
    pub fn end_frame(&self) -> u64 {
        self.start_frame.saturating_add(self.frame_count as u64)
    }
}

/// Conditions that affected one emitted window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowFlags {
    pub gaps: Vec<Gap>,
    pub emitted_after_max_skew: bool,
    pub final_drain: bool,
}

impl WindowFlags {
    pub fn has_gap_for(&self, track: AudioTrack) -> bool {
        self.gaps.iter().any(|gap| gap.track == track)
    }
}

/// One committed window on the shared, monotonically increasing frame clock.
#[derive(Debug, Clone, PartialEq)]
pub struct AlignedAudioWindow {
    pub sample_rate: u32,
    pub start_frame: u64,
    pub frame_count: usize,
    pub microphone: Vec<f32>,
    pub system_audio: Vec<f32>,
    pub mixed: Vec<f32>,
    pub flags: WindowFlags,
}

impl AlignedAudioWindow {
    pub fn end_frame(&self) -> u64 {
        self.start_frame.saturating_add(self.frame_count as u64)
    }

    pub fn samples(&self, track: AudioTrack) -> &[f32] {
        match track {
            AudioTrack::Microphone => &self.microphone,
            AudioTrack::SystemAudio => &self.system_audio,
            AudioTrack::Mixed => &self.mixed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynchronizerError {
    EmptyTopology,
    MixedTrackIsOutputOnly,
    TrackNotCaptured(AudioTrack),
    FrameRangeOverflow,
}

impl fmt::Display for SynchronizerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTopology => write!(formatter, "at least one capture track must be enabled"),
            Self::MixedTrackIsOutputOnly => {
                write!(formatter, "the mixed track is output-only")
            }
            Self::TrackNotCaptured(track) => {
                write!(
                    formatter,
                    "{track:?} is not enabled in the capture topology"
                )
            }
            Self::FrameRangeOverflow => write!(formatter, "audio chunk frame range overflowed"),
        }
    }
}

impl Error for SynchronizerError {}

#[derive(Debug, Default)]
struct TrackBuffer {
    start_frame: u64,
    samples: VecDeque<f32>,
    present: VecDeque<bool>,
    /// Furthest frame boundary observed for this source, including a known
    /// discontinuity before a later chunk.
    observed_end_frame: u64,
}

impl TrackBuffer {
    fn insert(
        &mut self,
        committed_frame: u64,
        chunk_start_frame: u64,
        chunk: &[f32],
    ) -> Result<usize, SynchronizerError> {
        let chunk_end_frame = chunk_start_frame
            .checked_add(chunk.len() as u64)
            .ok_or(SynchronizerError::FrameRangeOverflow)?;

        if chunk.is_empty() {
            return Ok(0);
        }

        if chunk_end_frame <= committed_frame {
            return Ok(chunk.len());
        }

        let late_prefix = committed_frame
            .saturating_sub(chunk_start_frame)
            .min(chunk.len() as u64) as usize;
        let accepted_start = chunk_start_frame.max(committed_frame);
        let accepted = &chunk[late_prefix.min(chunk.len())..];

        if self.samples.is_empty() {
            self.start_frame = committed_frame;
        } else if self.start_frame < committed_frame {
            self.discard_before(committed_frame);
        }

        let required_offset = accepted_start
            .checked_sub(self.start_frame)
            .ok_or(SynchronizerError::FrameRangeOverflow)? as usize;
        while self.samples.len() < required_offset {
            self.samples.push_back(0.0);
            self.present.push_back(false);
        }

        for (sample_offset, sample) in accepted.iter().copied().enumerate() {
            let offset = required_offset
                .checked_add(sample_offset)
                .ok_or(SynchronizerError::FrameRangeOverflow)?;
            if offset < self.samples.len() {
                // First supplied value wins, but an out-of-order chunk may
                // still fill a placeholder until the window is committed.
                if !self.present[offset] {
                    self.samples[offset] = sample;
                    self.present[offset] = true;
                }
            } else {
                self.samples.push_back(sample);
                self.present.push_back(true);
            }
        }

        self.observed_end_frame = self.observed_end_frame.max(chunk_end_frame);
        Ok(late_prefix.min(chunk.len()))
    }

    fn discard_before(&mut self, frame: u64) {
        if self.samples.is_empty() {
            self.start_frame = frame;
            return;
        }

        let remove = frame.saturating_sub(self.start_frame) as usize;
        let remove = remove.min(self.samples.len());
        self.samples.drain(..remove);
        self.present.drain(..remove);
        self.start_frame = self.start_frame.saturating_add(remove as u64);

        if self.samples.is_empty() {
            self.start_frame = frame;
        }
    }

    fn contiguous_frames_from(&self, frame: u64) -> usize {
        let Some(offset) = frame
            .checked_sub(self.start_frame)
            .and_then(|offset| usize::try_from(offset).ok())
        else {
            return 0;
        };

        self.present
            .iter()
            .skip(offset)
            .take_while(|present| **present)
            .count()
    }

    fn has_observed_through(&self, frame: u64) -> bool {
        self.observed_end_frame >= frame
    }

    fn sample_at(&self, frame: u64) -> Option<f32> {
        let offset = frame.checked_sub(self.start_frame)? as usize;
        if self.present.get(offset).copied().unwrap_or(false) {
            self.samples.get(offset).copied()
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmissionReason {
    Ready,
    MaxSkew,
    FinalDrain,
}

/// Aligns microphone and system-audio chunks on one recording-relative clock.
///
/// `start_frame` passed to [`AudioSynchronizer::push_chunk`] is relative to
/// recording start at 48 kHz. Resampling and timestamp-to-frame conversion
/// intentionally live outside this component.
#[derive(Debug)]
pub struct AudioSynchronizer {
    topology: CaptureTopology,
    microphone: TrackBuffer,
    system_audio: TrackBuffer,
    next_start_frame: u64,
    late_frames_dropped: u64,
}

impl AudioSynchronizer {
    pub fn new(topology: CaptureTopology) -> Result<Self, SynchronizerError> {
        if topology.active_track_count() == 0 {
            return Err(SynchronizerError::EmptyTopology);
        }

        Ok(Self {
            topology,
            microphone: TrackBuffer::default(),
            system_audio: TrackBuffer::default(),
            next_start_frame: 0,
            late_frames_dropped: 0,
        })
    }

    pub const fn sample_rate(&self) -> u32 {
        DEFAULT_SAMPLE_RATE
    }

    pub const fn window_frames(&self) -> usize {
        DEFAULT_WINDOW_FRAMES
    }

    pub const fn max_skew_frames(&self) -> usize {
        DEFAULT_MAX_SKEW_FRAMES
    }

    pub const fn next_start_frame(&self) -> u64 {
        self.next_start_frame
    }

    pub const fn late_frames_dropped(&self) -> u64 {
        self.late_frames_dropped
    }

    /// Adds one capture chunk and returns every complete window that became
    /// committable. Chunks for already committed frames are trimmed/dropped.
    pub fn push_chunk(
        &mut self,
        track: AudioTrack,
        start_frame: u64,
        samples: &[f32],
    ) -> Result<Vec<AlignedAudioWindow>, SynchronizerError> {
        if track == AudioTrack::Mixed {
            return Err(SynchronizerError::MixedTrackIsOutputOnly);
        }
        if !self.topology.is_active(track) {
            return Err(SynchronizerError::TrackNotCaptured(track));
        }

        let committed_frame = self.next_start_frame;
        let dropped = self
            .buffer_mut(track)
            .insert(committed_frame, start_frame, samples)?;
        self.late_frames_dropped = self.late_frames_dropped.saturating_add(dropped as u64);

        Ok(self.emit_ready_windows())
    }

    /// Commits all remaining frames. The last returned window may be shorter
    /// than 50 ms; it is never silently discarded or padded past its real end.
    pub fn drain_final(&mut self) -> Vec<AlignedAudioWindow> {
        let final_end = self
            .active_observed_ends()
            .into_iter()
            .max()
            .unwrap_or(self.next_start_frame);
        let mut windows = Vec::new();

        while self.next_start_frame < final_end {
            let remaining = final_end.saturating_sub(self.next_start_frame) as usize;
            let frame_count = remaining.min(DEFAULT_WINDOW_FRAMES);
            windows.push(self.commit_window(frame_count, EmissionReason::FinalDrain));
        }

        windows
    }

    fn emit_ready_windows(&mut self) -> Vec<AlignedAudioWindow> {
        let mut windows = Vec::new();
        loop {
            let Some(reason) = self.next_emission_reason() else {
                break;
            };
            windows.push(self.commit_window(DEFAULT_WINDOW_FRAMES, reason));
        }
        windows
    }

    fn next_emission_reason(&self) -> Option<EmissionReason> {
        let window_end = self
            .next_start_frame
            .saturating_add(DEFAULT_WINDOW_FRAMES as u64);

        if self.topology.active_track_count() == 1 {
            return self
                .active_buffers()
                .into_iter()
                .next()
                .filter(|buffer| buffer.has_observed_through(window_end))
                .map(|_| EmissionReason::Ready);
        }

        if self.microphone.has_observed_through(window_end)
            && self.system_audio.has_observed_through(window_end)
        {
            return Some(EmissionReason::Ready);
        }

        // Strictly greater than the budget: exactly 150 ms remains available
        // for the lagging callback to catch up without manufacturing a gap.
        let leader_buffered_frames = self
            .microphone
            .contiguous_frames_from(self.next_start_frame)
            .max(
                self.system_audio
                    .contiguous_frames_from(self.next_start_frame),
            );
        (leader_buffered_frames > DEFAULT_MAX_SKEW_FRAMES).then_some(EmissionReason::MaxSkew)
    }

    fn commit_window(
        &mut self,
        frame_count: usize,
        emission_reason: EmissionReason,
    ) -> AlignedAudioWindow {
        let start_frame = self.next_start_frame;
        let mut microphone = Vec::with_capacity(frame_count);
        let mut system_audio = Vec::with_capacity(frame_count);
        let mut microphone_present = Vec::with_capacity(frame_count);
        let mut system_audio_present = Vec::with_capacity(frame_count);

        self.collect_track(
            AudioTrack::Microphone,
            start_frame,
            frame_count,
            &mut microphone,
            &mut microphone_present,
        );
        self.collect_track(
            AudioTrack::SystemAudio,
            start_frame,
            frame_count,
            &mut system_audio,
            &mut system_audio_present,
        );

        let mixed = microphone
            .iter()
            .zip(system_audio.iter())
            .zip(microphone_present.iter().zip(system_audio_present.iter()))
            .map(
                |((&mic, &system), (&has_mic, &has_system))| match (has_mic, has_system) {
                    (true, true) => (mic + system) * 0.5,
                    (true, false) => mic,
                    (false, true) => system,
                    (false, false) => 0.0,
                },
            )
            .collect();

        let mut flags = WindowFlags {
            emitted_after_max_skew: emission_reason == EmissionReason::MaxSkew,
            final_drain: emission_reason == EmissionReason::FinalDrain,
            ..WindowFlags::default()
        };
        flags.gaps.extend(self.describe_gaps(
            AudioTrack::Microphone,
            start_frame,
            &microphone_present,
            emission_reason,
        ));
        flags.gaps.extend(self.describe_gaps(
            AudioTrack::SystemAudio,
            start_frame,
            &system_audio_present,
            emission_reason,
        ));

        self.next_start_frame = self.next_start_frame.saturating_add(frame_count as u64);
        self.microphone.discard_before(self.next_start_frame);
        self.system_audio.discard_before(self.next_start_frame);

        AlignedAudioWindow {
            sample_rate: DEFAULT_SAMPLE_RATE,
            start_frame,
            frame_count,
            microphone,
            system_audio,
            mixed,
            flags,
        }
    }

    fn collect_track(
        &self,
        track: AudioTrack,
        start_frame: u64,
        frame_count: usize,
        samples: &mut Vec<f32>,
        present: &mut Vec<bool>,
    ) {
        let active = self.topology.is_active(track);
        let buffer = self.buffer(track);
        for offset in 0..frame_count {
            let sample = active
                .then(|| buffer.sample_at(start_frame.saturating_add(offset as u64)))
                .flatten();
            samples.push(sample.unwrap_or(0.0));
            present.push(sample.is_some());
        }
    }

    fn describe_gaps(
        &self,
        track: AudioTrack,
        start_frame: u64,
        present: &[bool],
        emission_reason: EmissionReason,
    ) -> Vec<Gap> {
        let mut gaps = Vec::new();
        let mut cursor = 0;
        while cursor < present.len() {
            if present[cursor] {
                cursor += 1;
                continue;
            }

            let gap_start = cursor;
            let reason = self.gap_reason_at(
                track,
                start_frame.saturating_add(cursor as u64),
                emission_reason,
            );
            while cursor < present.len()
                && !present[cursor]
                && self.gap_reason_at(
                    track,
                    start_frame.saturating_add(cursor as u64),
                    emission_reason,
                ) == reason
            {
                cursor += 1;
            }

            let absolute_start = start_frame.saturating_add(gap_start as u64);
            gaps.push(Gap {
                track,
                start_frame: absolute_start,
                frame_count: cursor - gap_start,
                reason,
            });
        }
        gaps
    }

    fn gap_reason_at(
        &self,
        track: AudioTrack,
        frame: u64,
        emission_reason: EmissionReason,
    ) -> GapReason {
        if !self.topology.is_active(track) {
            GapReason::TrackNotCaptured
        } else if frame < self.buffer(track).observed_end_frame {
            GapReason::InputDiscontinuity
        } else {
            match emission_reason {
                EmissionReason::Ready => GapReason::InputDiscontinuity,
                EmissionReason::MaxSkew => GapReason::MaxSkewExceeded,
                EmissionReason::FinalDrain => GapReason::FinalDrain,
            }
        }
    }

    fn buffer(&self, track: AudioTrack) -> &TrackBuffer {
        match track {
            AudioTrack::Microphone => &self.microphone,
            AudioTrack::SystemAudio => &self.system_audio,
            AudioTrack::Mixed => unreachable!("mixed audio has no input buffer"),
        }
    }

    fn buffer_mut(&mut self, track: AudioTrack) -> &mut TrackBuffer {
        match track {
            AudioTrack::Microphone => &mut self.microphone,
            AudioTrack::SystemAudio => &mut self.system_audio,
            AudioTrack::Mixed => unreachable!("mixed audio has no input buffer"),
        }
    }

    fn active_buffers(&self) -> Vec<&TrackBuffer> {
        let mut buffers = Vec::with_capacity(self.topology.active_track_count());
        if self.topology.microphone {
            buffers.push(&self.microphone);
        }
        if self.topology.system_audio {
            buffers.push(&self.system_audio);
        }
        buffers
    }

    fn active_observed_ends(&self) -> Vec<u64> {
        self.active_buffers()
            .into_iter()
            .map(|buffer| buffer.observed_end_frame)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(value: f32, count: usize) -> Vec<f32> {
        vec![value; count]
    }

    #[test]
    fn aligns_different_chunk_boundaries_before_emitting() {
        let mut synchronizer = AudioSynchronizer::new(CaptureTopology::dual()).unwrap();

        assert!(synchronizer
            .push_chunk(AudioTrack::Microphone, 0, &samples(0.4, 700))
            .unwrap()
            .is_empty());
        assert!(synchronizer
            .push_chunk(AudioTrack::SystemAudio, 0, &samples(0.2, 1_100))
            .unwrap()
            .is_empty());
        assert!(synchronizer
            .push_chunk(AudioTrack::Microphone, 700, &samples(0.4, 1_700))
            .unwrap()
            .is_empty());

        let windows = synchronizer
            .push_chunk(AudioTrack::SystemAudio, 1_100, &samples(0.2, 1_300))
            .unwrap();
        assert_eq!(windows.len(), 1);
        let window = &windows[0];
        assert_eq!(window.start_frame, 0);
        assert_eq!(window.frame_count, DEFAULT_WINDOW_FRAMES);
        assert!(window.microphone.iter().all(|sample| *sample == 0.4));
        assert!(window.system_audio.iter().all(|sample| *sample == 0.2));
        assert!(window
            .mixed
            .iter()
            .all(|sample| (*sample - 0.3).abs() < f32::EPSILON));
        assert!(window.flags.gaps.is_empty());
    }

    #[test]
    fn single_microphone_and_system_topologies_emit_without_skew_wait() {
        for (topology, track) in [
            (CaptureTopology::microphone_only(), AudioTrack::Microphone),
            (
                CaptureTopology::system_audio_only(),
                AudioTrack::SystemAudio,
            ),
        ] {
            let mut synchronizer = AudioSynchronizer::new(topology).unwrap();
            let windows = synchronizer
                .push_chunk(track, 0, &samples(0.5, DEFAULT_WINDOW_FRAMES))
                .unwrap();

            assert_eq!(windows.len(), 1);
            assert!(windows[0]
                .samples(track)
                .iter()
                .all(|sample| *sample == 0.5));
            assert!(windows[0].mixed.iter().all(|sample| *sample == 0.5));
            let inactive = if track == AudioTrack::Microphone {
                AudioTrack::SystemAudio
            } else {
                AudioTrack::Microphone
            };
            assert!(windows[0].flags.has_gap_for(inactive));
            assert!(windows[0]
                .flags
                .gaps
                .iter()
                .filter(|gap| gap.track == inactive)
                .all(|gap| gap.reason == GapReason::TrackNotCaptured));
        }
    }

    #[test]
    fn max_skew_emits_gap_and_late_samples_cannot_backfill() {
        let mut synchronizer = AudioSynchronizer::new(CaptureTopology::dual()).unwrap();

        let at_budget = synchronizer
            .push_chunk(
                AudioTrack::Microphone,
                0,
                &samples(0.7, DEFAULT_MAX_SKEW_FRAMES),
            )
            .unwrap();
        assert!(at_budget.is_empty());

        let windows = synchronizer
            .push_chunk(
                AudioTrack::Microphone,
                DEFAULT_MAX_SKEW_FRAMES as u64,
                &samples(0.7, DEFAULT_WINDOW_FRAMES),
            )
            .unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].start_frame, 0);
        assert!(windows[0].flags.emitted_after_max_skew);
        assert!(windows[0].flags.gaps.iter().any(|gap| {
            gap.track == AudioTrack::SystemAudio
                && gap.reason == GapReason::MaxSkewExceeded
                && gap.frame_count == DEFAULT_WINDOW_FRAMES
        }));

        // These samples belong to an already committed window. They must be
        // dropped, not used to rewrite or re-emit frame zero.
        let late = synchronizer
            .push_chunk(
                AudioTrack::SystemAudio,
                0,
                &samples(0.3, DEFAULT_WINDOW_FRAMES),
            )
            .unwrap();
        assert!(late.is_empty());
        assert_eq!(
            synchronizer.late_frames_dropped(),
            DEFAULT_WINDOW_FRAMES as u64
        );
        assert_eq!(
            synchronizer.next_start_frame(),
            DEFAULT_WINDOW_FRAMES as u64
        );
    }

    #[test]
    fn final_drain_preserves_short_tail() {
        let mut synchronizer = AudioSynchronizer::new(CaptureTopology::dual()).unwrap();
        let tail_frames = DEFAULT_WINDOW_FRAMES / 3;
        synchronizer
            .push_chunk(AudioTrack::Microphone, 0, &samples(0.8, tail_frames))
            .unwrap();
        synchronizer
            .push_chunk(AudioTrack::SystemAudio, 0, &samples(0.4, tail_frames / 2))
            .unwrap();

        let windows = synchronizer.drain_final();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].start_frame, 0);
        assert_eq!(windows[0].frame_count, tail_frames);
        assert!(windows[0].flags.final_drain);
        assert_eq!(windows[0].microphone.len(), tail_frames);
        assert_eq!(windows[0].system_audio.len(), tail_frames);
        assert!(windows[0].flags.gaps.iter().any(|gap| {
            gap.track == AudioTrack::SystemAudio
                && gap.reason == GapReason::FinalDrain
                && gap.frame_count == tail_frames - tail_frames / 2
        }));
    }

    #[test]
    fn emitted_start_frames_are_strictly_monotonic() {
        let mut synchronizer = AudioSynchronizer::new(CaptureTopology::microphone_only()).unwrap();
        let full_and_tail = DEFAULT_WINDOW_FRAMES * 3 + 17;

        let mut windows = synchronizer
            .push_chunk(AudioTrack::Microphone, 0, &samples(0.1, full_and_tail))
            .unwrap();
        windows.extend(synchronizer.drain_final());

        assert_eq!(windows.len(), 4);
        assert_eq!(windows[3].frame_count, 17);
        for pair in windows.windows(2) {
            assert_eq!(pair[0].end_frame(), pair[1].start_frame);
            assert!(pair[0].start_frame < pair[1].start_frame);
        }
        assert_eq!(synchronizer.next_start_frame(), full_and_tail as u64);
    }
}
