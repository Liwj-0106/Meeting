//! Low-volume health transitions derived from per-window synchronization gaps.
//!
//! The synchronizer can report a gap every 50 ms while a source is missing.
//! Persisting every window would turn a health log into another audio-rate hot
//! path. This reducer emits only the first degraded transition and the later
//! recovery, while retaining the complete missing-frame count on recovery.

use super::synchronizer::{AudioTrack as SynchronizerTrack, Gap, GapReason};
use super::timeline_event::{
    AudioEventSeverity, AudioTimelineEventDraft, AudioTimelineEventKind,
    AudioTrack as TimelineTrack, AudioTrackState,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveGap {
    end_frame: u64,
    total_frames: u64,
    reason: GapReason,
}

#[derive(Debug, Default)]
pub struct GapHealthTracker {
    microphone: Option<ActiveGap>,
    system_audio: Option<ActiveGap>,
}

impl GapHealthTracker {
    pub fn observe_window(
        &mut self,
        start_frame: u64,
        end_frame: u64,
        gaps: &[Gap],
    ) -> Vec<AudioTimelineEventDraft> {
        let mut drafts = Vec::with_capacity(2);
        Self::observe_track(
            &mut self.microphone,
            SynchronizerTrack::Microphone,
            TimelineTrack::Microphone,
            start_frame,
            end_frame,
            gaps,
            &mut drafts,
        );
        Self::observe_track(
            &mut self.system_audio,
            SynchronizerTrack::SystemAudio,
            TimelineTrack::System,
            start_frame,
            end_frame,
            gaps,
            &mut drafts,
        );
        drafts
    }

    /// Close any gap that is still active when the session ends.
    ///
    /// There is no later healthy window to trigger the ordinary recovery
    /// transition, so emit one terminal summary carrying the complete missing
    /// frame count instead of leaving only the first 50 ms in diagnostics.
    pub fn finish_session(&mut self, at_frame: u64) -> Vec<AudioTimelineEventDraft> {
        let mut drafts = Vec::with_capacity(2);
        Self::finish_track(
            &mut self.microphone,
            TimelineTrack::Microphone,
            at_frame,
            &mut drafts,
        );
        Self::finish_track(
            &mut self.system_audio,
            TimelineTrack::System,
            at_frame,
            &mut drafts,
        );
        drafts
    }

    fn finish_track(
        active: &mut Option<ActiveGap>,
        timeline_track: TimelineTrack,
        at_frame: u64,
        drafts: &mut Vec<AudioTimelineEventDraft>,
    ) {
        let Some(completed) = active.take() else {
            return;
        };
        let expected_tail = completed.reason == GapReason::FinalDrain;
        let mut terminal = AudioTimelineEventDraft::new(
            timeline_track,
            AudioTimelineEventKind::GapDetected,
            if expected_tail {
                AudioTrackState::Healthy
            } else {
                AudioTrackState::Degraded
            },
            at_frame,
        );
        terminal.severity = if expected_tail {
            AudioEventSeverity::Info
        } else {
            AudioEventSeverity::Warning
        };
        terminal.code = if expected_tail {
            "audio_gap_final_drain_summary"
        } else {
            "audio_gap_unrecovered_at_stop"
        }
        .to_string();
        terminal.end_frame = Some(completed.end_frame);
        terminal.gap_frames = completed.total_frames;
        terminal.detail = Some(format!(
            "Session ended with {} missing frames ({:?})",
            completed.total_frames, completed.reason
        ));
        drafts.push(terminal);
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_track(
        active: &mut Option<ActiveGap>,
        synchronizer_track: SynchronizerTrack,
        timeline_track: TimelineTrack,
        start_frame: u64,
        end_frame: u64,
        gaps: &[Gap],
        drafts: &mut Vec<AudioTimelineEventDraft>,
    ) {
        let relevant: Vec<&Gap> = gaps
            .iter()
            .filter(|gap| {
                gap.track == synchronizer_track && gap.reason != GapReason::TrackNotCaptured
            })
            .collect();

        if relevant.is_empty() {
            if let Some(completed) = active.take() {
                let mut recovered = AudioTimelineEventDraft::new(
                    timeline_track,
                    AudioTimelineEventKind::StateChanged,
                    AudioTrackState::Healthy,
                    start_frame,
                );
                recovered.code = "audio_gap_recovered".to_string();
                recovered.end_frame = Some(completed.end_frame);
                recovered.gap_frames = completed.total_frames;
                recovered.detail = Some(format!(
                    "Audio resumed after {} missing frames ({:?})",
                    completed.total_frames, completed.reason
                ));
                drafts.push(recovered);
            }
            return;
        }

        let gap_frames = relevant
            .iter()
            .map(|gap| gap.frame_count as u64)
            .fold(0u64, u64::saturating_add);
        let reason = relevant[0].reason;
        let gap_start = relevant
            .iter()
            .map(|gap| gap.start_frame)
            .min()
            .unwrap_or(start_frame);
        let gap_end = relevant
            .iter()
            .map(|gap| gap.end_frame())
            .max()
            .unwrap_or(end_frame);

        if let Some(existing) = active.as_mut() {
            existing.end_frame = existing.end_frame.max(gap_end);
            existing.total_frames = existing.total_frames.saturating_add(gap_frames);
            return;
        }

        *active = Some(ActiveGap {
            end_frame: gap_end,
            total_frames: gap_frames,
            reason,
        });

        let expected_tail = reason == GapReason::FinalDrain;
        let mut detected = AudioTimelineEventDraft::new(
            timeline_track,
            AudioTimelineEventKind::GapDetected,
            if expected_tail {
                AudioTrackState::Healthy
            } else {
                AudioTrackState::Degraded
            },
            gap_start,
        );
        detected.severity = if expected_tail {
            AudioEventSeverity::Info
        } else {
            AudioEventSeverity::Warning
        };
        detected.code = match reason {
            GapReason::MaxSkewExceeded => "audio_gap_max_skew",
            GapReason::InputDiscontinuity => "audio_gap_input_discontinuity",
            GapReason::FinalDrain => "audio_gap_final_drain",
            GapReason::TrackNotCaptured => unreachable!("disabled tracks are filtered above"),
        }
        .to_string();
        detected.end_frame = Some(gap_end);
        detected.gap_frames = gap_frames;
        detected.detail = Some(format!(
            "The missing range was represented as silence ({reason:?})"
        ));
        drafts.push(detected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gap(track: SynchronizerTrack, start_frame: u64, reason: GapReason) -> Gap {
        Gap {
            track,
            start_frame,
            frame_count: 2_400,
            reason,
        }
    }

    #[test]
    fn consecutive_gap_windows_emit_one_degraded_and_one_recovery() {
        let mut tracker = GapHealthTracker::default();
        let first = tracker.observe_window(
            0,
            2_400,
            &[gap(
                SynchronizerTrack::SystemAudio,
                0,
                GapReason::MaxSkewExceeded,
            )],
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kind, AudioTimelineEventKind::GapDetected);
        assert_eq!(first[0].state, AudioTrackState::Degraded);

        assert!(tracker
            .observe_window(
                2_400,
                4_800,
                &[gap(
                    SynchronizerTrack::SystemAudio,
                    2_400,
                    GapReason::MaxSkewExceeded,
                )],
            )
            .is_empty());

        let recovered = tracker.observe_window(4_800, 7_200, &[]);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].code, "audio_gap_recovered");
        assert_eq!(recovered[0].gap_frames, 4_800);
        assert_eq!(recovered[0].state, AudioTrackState::Healthy);
    }

    #[test]
    fn disabled_track_gaps_are_ignored_and_final_tail_is_informational() {
        let mut tracker = GapHealthTracker::default();
        assert!(tracker
            .observe_window(
                0,
                2_400,
                &[gap(
                    SynchronizerTrack::Microphone,
                    0,
                    GapReason::TrackNotCaptured,
                )],
            )
            .is_empty());

        let tail = tracker.observe_window(
            2_400,
            4_800,
            &[gap(
                SynchronizerTrack::Microphone,
                2_400,
                GapReason::FinalDrain,
            )],
        );
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].severity, AudioEventSeverity::Info);
        assert_eq!(tail[0].state, AudioTrackState::Healthy);
    }

    #[test]
    fn session_end_summarizes_an_unrecovered_gap() {
        let mut tracker = GapHealthTracker::default();
        tracker.observe_window(
            0,
            2_400,
            &[gap(
                SynchronizerTrack::SystemAudio,
                0,
                GapReason::MaxSkewExceeded,
            )],
        );
        tracker.observe_window(
            2_400,
            4_800,
            &[gap(
                SynchronizerTrack::SystemAudio,
                2_400,
                GapReason::MaxSkewExceeded,
            )],
        );

        let terminal = tracker.finish_session(4_800);
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].code, "audio_gap_unrecovered_at_stop");
        assert_eq!(terminal[0].gap_frames, 4_800);
        assert_eq!(terminal[0].state, AudioTrackState::Degraded);
        assert!(tracker.finish_session(4_800).is_empty());
    }
}
