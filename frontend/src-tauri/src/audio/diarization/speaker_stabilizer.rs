use super::moss_protocol::MossSegment;
use crate::audio::transcription::{SpeakerMetadata, SpeakerStatus};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct StableSpeakerProfile {
    pub speaker_id: String,
    pub display_name: Option<String>,
    pub status: SpeakerStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StabilizedObservation {
    pub start_frame: u64,
    pub end_frame: u64,
    pub text: String,
    pub speaker: SpeakerMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerAssignment {
    pub local_label: String,
    pub speaker: SpeakerMetadata,
    /// Similarity used only for deterministic window matching. It is not a
    /// model-provided speaker confidence and must not be persisted as one.
    pub match_score: Option<f64>,
    pub is_new: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StabilizationResult {
    pub assignments: Vec<SpeakerAssignment>,
    pub observations: Vec<StabilizedObservation>,
    pub next_speaker_index: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerStabilizerConfig {
    pub time_iou_weight: f64,
    pub text_similarity_weight: f64,
    pub minimum_match_score: f64,
    pub speaker_id_prefix: String,
    pub next_speaker_index: u64,
}

impl Default for SpeakerStabilizerConfig {
    fn default() -> Self {
        Self {
            time_iou_weight: 0.7,
            text_similarity_weight: 0.3,
            minimum_match_score: 0.35,
            speaker_id_prefix: "speaker".to_string(),
            next_speaker_index: 1,
        }
    }
}

/// Stabilize window-local MOSS labels against already materialized overlap.
///
/// Matching is one-to-one and deterministic: candidates are ordered by score,
/// then local label and stable speaker ID. Existing speaker profiles are never
/// mutated; protected user states (`user_confirmed`, `renamed`, `merged`) are
/// carried into new observations unchanged.
pub fn stabilize_speakers(
    previous_overlap: &[StabilizedObservation],
    current: &[MossSegment],
    config: &SpeakerStabilizerConfig,
) -> StabilizationResult {
    let current_groups = group_current(current);
    let previous_groups = group_previous(previous_overlap);
    let profiles = collect_profiles(previous_overlap);
    let mut candidates = Vec::new();

    for (local_label, current_segments) in &current_groups {
        for (speaker_id, previous_segments) in &previous_groups {
            let score = group_match_score(current_segments, previous_segments, config);
            if score >= config.minimum_match_score {
                candidates.push(Candidate {
                    local_label: local_label.clone(),
                    speaker_id: speaker_id.clone(),
                    score,
                });
            }
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.local_label.cmp(&right.local_label))
            .then_with(|| left.speaker_id.cmp(&right.speaker_id))
    });

    let mut matched_labels = HashSet::new();
    let mut matched_speakers = HashSet::new();
    let mut selected: BTreeMap<String, (String, f64)> = BTreeMap::new();
    for candidate in candidates {
        if matched_labels.contains(&candidate.local_label)
            || matched_speakers.contains(&candidate.speaker_id)
        {
            continue;
        }
        matched_labels.insert(candidate.local_label.clone());
        matched_speakers.insert(candidate.speaker_id.clone());
        selected.insert(
            candidate.local_label,
            (candidate.speaker_id, candidate.score),
        );
    }

    let mut used_speaker_ids: BTreeSet<String> = previous_groups.keys().cloned().collect();
    let mut next_index = config.next_speaker_index.max(1);
    let mut assignments = Vec::with_capacity(current_groups.len());
    for local_label in current_groups.keys() {
        let assignment = if let Some((speaker_id, score)) = selected.get(local_label) {
            let profile =
                profiles
                    .get(speaker_id)
                    .cloned()
                    .unwrap_or_else(|| StableSpeakerProfile {
                        speaker_id: speaker_id.clone(),
                        display_name: None,
                        status: SpeakerStatus::Provisional,
                    });
            SpeakerAssignment {
                local_label: local_label.clone(),
                speaker: SpeakerMetadata {
                    speaker_id: profile.speaker_id,
                    local_label: Some(local_label.clone()),
                    display_name: profile.display_name,
                    // MOSS-Transcribe-Diarize does not return speaker confidence.
                    confidence: None,
                    status: profile.status,
                },
                match_score: Some(*score),
                is_new: false,
            }
        } else {
            let speaker_id = loop {
                let candidate = format!("{}-{next_index:03}", config.speaker_id_prefix);
                next_index = next_index.saturating_add(1);
                if used_speaker_ids.insert(candidate.clone()) {
                    break candidate;
                }
            };
            SpeakerAssignment {
                local_label: local_label.clone(),
                speaker: SpeakerMetadata {
                    speaker_id,
                    local_label: Some(local_label.clone()),
                    display_name: None,
                    confidence: None,
                    status: SpeakerStatus::Provisional,
                },
                match_score: None,
                is_new: true,
            }
        };
        assignments.push(assignment);
    }

    let assignment_by_label: HashMap<_, _> = assignments
        .iter()
        .map(|assignment| (assignment.local_label.as_str(), &assignment.speaker))
        .collect();
    let observations = current
        .iter()
        .filter_map(|segment| {
            assignment_by_label
                .get(segment.speaker.as_str())
                .map(|speaker| StabilizedObservation {
                    start_frame: segment.start_frame,
                    end_frame: segment.end_frame,
                    text: segment.text.clone(),
                    speaker: (*speaker).clone(),
                })
        })
        .collect();

    StabilizationResult {
        assignments,
        observations,
        next_speaker_index: next_index,
    }
}

#[derive(Debug)]
struct Candidate {
    local_label: String,
    speaker_id: String,
    score: f64,
}

fn group_current(current: &[MossSegment]) -> BTreeMap<String, Vec<&MossSegment>> {
    let mut groups: BTreeMap<String, Vec<&MossSegment>> = BTreeMap::new();
    for segment in current {
        groups
            .entry(segment.speaker.clone())
            .or_default()
            .push(segment);
    }
    groups
}

fn group_previous(
    previous: &[StabilizedObservation],
) -> BTreeMap<String, Vec<&StabilizedObservation>> {
    let mut groups: BTreeMap<String, Vec<&StabilizedObservation>> = BTreeMap::new();
    for observation in previous {
        groups
            .entry(observation.speaker.speaker_id.clone())
            .or_default()
            .push(observation);
    }
    groups
}

fn collect_profiles(
    observations: &[StabilizedObservation],
) -> BTreeMap<String, StableSpeakerProfile> {
    let mut profiles = BTreeMap::new();
    for observation in observations {
        let candidate = StableSpeakerProfile {
            speaker_id: observation.speaker.speaker_id.clone(),
            display_name: observation.speaker.display_name.clone(),
            status: observation.speaker.status.clone(),
        };
        profiles
            .entry(candidate.speaker_id.clone())
            .and_modify(|existing| {
                if profile_priority(&candidate).cmp(&profile_priority(existing))
                    == Ordering::Greater
                {
                    *existing = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    profiles
}

fn profile_priority(profile: &StableSpeakerProfile) -> (u8, u8, &str) {
    let status_rank = match profile.status {
        SpeakerStatus::Merged => 6,
        SpeakerStatus::Renamed => 5,
        SpeakerStatus::UserConfirmed => 4,
        SpeakerStatus::Resolved => 3,
        SpeakerStatus::Provisional => 2,
        SpeakerStatus::Unresolved => 1,
        SpeakerStatus::Unknown(_) => 0,
    };
    (
        status_rank,
        profile.display_name.is_some() as u8,
        profile.display_name.as_deref().unwrap_or(""),
    )
}

fn group_match_score(
    current: &[&MossSegment],
    previous: &[&StabilizedObservation],
    config: &SpeakerStabilizerConfig,
) -> f64 {
    let mut scores = Vec::new();
    for current_segment in current {
        let best = previous
            .iter()
            .map(|previous_segment| {
                let time_iou = interval_iou(
                    current_segment.start_frame,
                    current_segment.end_frame,
                    previous_segment.start_frame,
                    previous_segment.end_frame,
                );
                if time_iou == 0.0 {
                    return 0.0;
                }
                config.time_iou_weight * time_iou
                    + config.text_similarity_weight
                        * text_similarity(&current_segment.text, &previous_segment.text)
            })
            .fold(0.0, f64::max);
        if best > 0.0 {
            scores.push(best);
        }
    }
    if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f64>() / scores.len() as f64
    }
}

fn interval_iou(left_start: u64, left_end: u64, right_start: u64, right_end: u64) -> f64 {
    let intersection = left_end
        .min(right_end)
        .saturating_sub(left_start.max(right_start));
    if intersection == 0 {
        return 0.0;
    }
    let union = left_end
        .max(right_end)
        .saturating_sub(left_start.min(right_start));
    intersection as f64 / union as f64
}

fn text_similarity(left: &str, right: &str) -> f64 {
    let left = normalize_text(left);
    let right = normalize_text(right);
    let longest = left.len().max(right.len());
    if longest == 0 {
        return 1.0;
    }
    let distance = levenshtein(&left, &right);
    1.0 - distance as f64 / longest as f64
}

fn normalize_text(value: &str) -> Vec<char> {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn levenshtein(left: &[char], right: &[char]) -> usize {
    if left.is_empty() {
        return right.len();
    }
    if right.is_empty() {
        return left.len();
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (left_index, left_character) in left.iter().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_character) in right.iter().enumerate() {
            let substitution =
                previous[right_index] + usize::from(left_character != right_character);
            current[right_index + 1] = (current[right_index] + 1)
                .min(previous[right_index + 1] + 1)
                .min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn previous(
        start_frame: u64,
        end_frame: u64,
        text: &str,
        speaker_id: &str,
        display_name: Option<&str>,
        status: SpeakerStatus,
    ) -> StabilizedObservation {
        StabilizedObservation {
            start_frame,
            end_frame,
            text: text.to_string(),
            speaker: SpeakerMetadata {
                speaker_id: speaker_id.to_string(),
                local_label: Some("old-local".to_string()),
                display_name: display_name.map(str::to_string),
                confidence: None,
                status,
            },
        }
    }

    fn current(start: u64, end: u64, speaker: &str, text: &str) -> MossSegment {
        MossSegment {
            start_frame: start,
            end_frame: end,
            speaker: speaker.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn swapped_local_labels_map_by_overlap_and_text() {
        let previous = vec![
            previous(
                0,
                1_000,
                "先确认接口边界",
                "speaker-001",
                Some("Alice"),
                SpeakerStatus::Renamed,
            ),
            previous(
                1_000,
                2_000,
                "补充失败隔离方案",
                "speaker-002",
                None,
                SpeakerStatus::Resolved,
            ),
        ];
        let current = vec![
            current(100, 1_000, "S99", "先确认接口边界。"),
            current(1_000, 1_900, "S01", "补充失败隔离方案。"),
        ];

        let result = stabilize_speakers(&previous, &current, &Default::default());
        let by_local: HashMap<_, _> = result
            .assignments
            .iter()
            .map(|item| (item.local_label.as_str(), &item.speaker))
            .collect();
        assert_eq!(by_local["S99"].speaker_id, "speaker-001");
        assert_eq!(by_local["S99"].display_name.as_deref(), Some("Alice"));
        assert_eq!(by_local["S99"].status, SpeakerStatus::Renamed);
        assert_eq!(by_local["S01"].speaker_id, "speaker-002");
        assert!(result.assignments.iter().all(|item| !item.is_new));
    }

    #[test]
    fn protected_user_metadata_is_not_overwritten_or_given_fake_confidence() {
        let previous = vec![previous(
            0,
            1_000,
            "确认完成",
            "speaker-007",
            Some("用户命名"),
            SpeakerStatus::UserConfirmed,
        )];
        let result = stabilize_speakers(
            &previous,
            &[current(0, 1_000, "S01", "确认完成")],
            &Default::default(),
        );
        let speaker = &result.assignments[0].speaker;
        assert_eq!(speaker.speaker_id, "speaker-007");
        assert_eq!(speaker.display_name.as_deref(), Some("用户命名"));
        assert_eq!(speaker.status, SpeakerStatus::UserConfirmed);
        assert_eq!(speaker.confidence, None);
        assert_eq!(
            previous[0].speaker.local_label.as_deref(),
            Some("old-local")
        );
    }

    #[test]
    fn no_temporal_overlap_creates_a_new_provisional_speaker() {
        let previous = vec![previous(
            0,
            1_000,
            "重复短句",
            "speaker-001",
            None,
            SpeakerStatus::Resolved,
        )];
        let result = stabilize_speakers(
            &previous,
            &[current(2_000, 3_000, "S01", "重复短句")],
            &SpeakerStabilizerConfig {
                next_speaker_index: 1,
                ..Default::default()
            },
        );
        let assignment = &result.assignments[0];
        assert!(assignment.is_new);
        assert_eq!(assignment.speaker.speaker_id, "speaker-002");
        assert_eq!(assignment.speaker.status, SpeakerStatus::Provisional);
        assert_eq!(assignment.match_score, None);
    }

    #[test]
    fn equal_scores_have_a_stable_lexical_tie_break() {
        let previous = vec![
            previous(
                0,
                1_000,
                "同一句",
                "speaker-b",
                None,
                SpeakerStatus::Resolved,
            ),
            previous(
                0,
                1_000,
                "同一句",
                "speaker-a",
                None,
                SpeakerStatus::Resolved,
            ),
        ];
        let current = vec![current(0, 1_000, "S01", "同一句")];
        for _ in 0..5 {
            let result = stabilize_speakers(&previous, &current, &Default::default());
            assert_eq!(result.assignments[0].speaker.speaker_id, "speaker-a");
        }
    }

    #[test]
    fn unicode_text_similarity_is_character_based() {
        assert!(text_similarity("实时 会议纪要", "实时会议纪要。") > 0.8);
        assert_eq!(interval_iou(0, 10, 20, 30), 0.0);
        assert_eq!(interval_iou(0, 10, 0, 10), 1.0);
    }
}
