use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

pub const MOSS_WORKER_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MossProtocolLimits {
    pub max_window_frames: u64,
    pub max_segments: usize,
    pub max_text_chars_per_segment: usize,
    pub max_total_text_chars: usize,
    pub max_prompt_chars: usize,
    pub max_jsonl_bytes: usize,
}

impl Default for MossProtocolLimits {
    fn default() -> Self {
        Self {
            // 90 seconds at the worker contract's expected 48 kHz snapshot.
            max_window_frames: 90 * 48_000,
            max_segments: 4_096,
            max_text_chars_per_segment: 16_384,
            max_total_text_chars: 1_048_576,
            max_prompt_chars: 4_096,
            max_jsonl_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MossWorkerRequest {
    pub schema: u16,
    pub job: String,
    pub session: String,
    pub window_start_frame: u64,
    pub window_end_frame: u64,
    pub audio_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub model_revision: String,
}

impl fmt::Debug for MossWorkerRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossWorkerRequest")
            .field("schema", &self.schema)
            .field("job", &self.job)
            .field("session", &self.session)
            .field("window_start_frame", &self.window_start_frame)
            .field("window_end_frame", &self.window_end_frame)
            .field("audio_path", &"<redacted>")
            .field(
                "prompt",
                &self
                    .prompt
                    .as_ref()
                    .map(|value| format!("<redacted:{} chars>", value.chars().count())),
            )
            .field("model_revision", &self.model_revision)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MossSegment {
    pub start_frame: u64,
    pub end_frame: u64,
    pub speaker: String,
    pub text: String,
}

impl fmt::Debug for MossSegment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossSegment")
            .field("start_frame", &self.start_frame)
            .field("end_frame", &self.end_frame)
            .field("speaker", &self.speaker)
            .field(
                "text",
                &format!("<redacted:{} chars>", self.text.chars().count()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MossWorkerResponse {
    pub schema: u16,
    pub job: String,
    pub session: String,
    pub window_start_frame: u64,
    pub window_end_frame: u64,
    pub segments: Vec<MossSegment>,
}

impl fmt::Debug for MossWorkerResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MossWorkerResponse")
            .field("schema", &self.schema)
            .field("job", &self.job)
            .field("session", &self.session)
            .field("window_start_frame", &self.window_start_frame)
            .field("window_end_frame", &self.window_end_frame)
            .field("segment_count", &self.segments.len())
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MossProtocolError {
    #[error("invalid {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("audio_path is outside the configured worker audio root")]
    PathOutsideRoot,
    #[error("JSONL message exceeds the configured byte limit")]
    LineTooLong,
    #[error("JSONL message must contain exactly one physical line")]
    MultipleLines,
    #[error("invalid JSONL message at line {line}, column {column}")]
    InvalidJson { line: usize, column: usize },
    #[error("response does not match its request: {field}")]
    ResponseMismatch { field: &'static str },
    #[error("segments are not sorted by start/end/speaker")]
    UnsortedSegments,
    #[error("segments for one local speaker overlap")]
    SameSpeakerOverlap,
}

impl MossWorkerRequest {
    pub fn validate(
        &self,
        allowed_audio_root: &Path,
        limits: MossProtocolLimits,
    ) -> Result<(), MossProtocolError> {
        validate_schema(self.schema)?;
        validate_identifier("job", &self.job, 128)?;
        validate_identifier("session", &self.session, 128)?;
        validate_window(
            self.window_start_frame,
            self.window_end_frame,
            limits.max_window_frames,
        )?;
        validate_audio_path(&self.audio_path, allowed_audio_root)?;
        validate_revision(&self.model_revision)?;
        if let Some(prompt) = &self.prompt {
            validate_human_text("prompt", prompt, limits.max_prompt_chars, true)?;
        }
        Ok(())
    }

    pub fn to_json_line(
        &self,
        allowed_audio_root: &Path,
        limits: MossProtocolLimits,
    ) -> Result<String, MossProtocolError> {
        self.validate(allowed_audio_root, limits)?;
        serialize_line(self, limits)
    }

    pub fn from_json_line(
        line: &str,
        allowed_audio_root: &Path,
        limits: MossProtocolLimits,
    ) -> Result<Self, MossProtocolError> {
        let request: Self = deserialize_line(line, limits)?;
        request.validate(allowed_audio_root, limits)?;
        Ok(request)
    }
}

impl MossWorkerResponse {
    pub fn validate_for_request(
        &self,
        request: &MossWorkerRequest,
        limits: MossProtocolLimits,
    ) -> Result<(), MossProtocolError> {
        validate_schema(self.schema)?;
        validate_identifier("job", &self.job, 128)?;
        validate_identifier("session", &self.session, 128)?;
        for (field, matches) in [
            ("schema", self.schema == request.schema),
            ("job", self.job == request.job),
            ("session", self.session == request.session),
            (
                "window_start_frame",
                self.window_start_frame == request.window_start_frame,
            ),
            (
                "window_end_frame",
                self.window_end_frame == request.window_end_frame,
            ),
        ] {
            if !matches {
                return Err(MossProtocolError::ResponseMismatch { field });
            }
        }
        validate_window(
            self.window_start_frame,
            self.window_end_frame,
            limits.max_window_frames,
        )?;
        if self.segments.len() > limits.max_segments {
            return Err(MossProtocolError::InvalidField {
                field: "segments",
                reason: "too many segments",
            });
        }

        let mut previous_key: Option<(u64, u64, &str)> = None;
        let mut last_end_by_speaker: HashMap<&str, u64> = HashMap::new();
        let mut total_text_chars = 0usize;
        for segment in &self.segments {
            if segment.start_frame < self.window_start_frame
                || segment.end_frame > self.window_end_frame
                || segment.end_frame <= segment.start_frame
            {
                return Err(MossProtocolError::InvalidField {
                    field: "segments.range",
                    reason: "segment must be non-empty and inside the request window",
                });
            }
            validate_speaker_label(&segment.speaker)?;
            validate_human_text(
                "segments.text",
                &segment.text,
                limits.max_text_chars_per_segment,
                false,
            )?;
            total_text_chars = total_text_chars.saturating_add(segment.text.chars().count());
            if total_text_chars > limits.max_total_text_chars {
                return Err(MossProtocolError::InvalidField {
                    field: "segments.text",
                    reason: "combined text is too long",
                });
            }

            let key = (
                segment.start_frame,
                segment.end_frame,
                segment.speaker.as_str(),
            );
            if previous_key.is_some_and(|previous| previous > key) {
                return Err(MossProtocolError::UnsortedSegments);
            }
            previous_key = Some(key);

            if last_end_by_speaker
                .get(segment.speaker.as_str())
                .is_some_and(|last_end| segment.start_frame < *last_end)
            {
                return Err(MossProtocolError::SameSpeakerOverlap);
            }
            last_end_by_speaker.insert(segment.speaker.as_str(), segment.end_frame);
        }
        Ok(())
    }

    pub fn to_json_line(
        &self,
        request: &MossWorkerRequest,
        limits: MossProtocolLimits,
    ) -> Result<String, MossProtocolError> {
        self.validate_for_request(request, limits)?;
        serialize_line(self, limits)
    }

    pub fn from_json_line(
        line: &str,
        request: &MossWorkerRequest,
        limits: MossProtocolLimits,
    ) -> Result<Self, MossProtocolError> {
        let response: Self = deserialize_line(line, limits)?;
        response.validate_for_request(request, limits)?;
        Ok(response)
    }
}

fn validate_schema(schema: u16) -> Result<(), MossProtocolError> {
    if schema != MOSS_WORKER_SCHEMA_VERSION {
        return Err(MossProtocolError::InvalidField {
            field: "schema",
            reason: "unsupported schema version",
        });
    }
    Ok(())
}

fn validate_identifier(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), MossProtocolError> {
    let length = value.chars().count();
    if length == 0 || length > max_chars {
        return Err(MossProtocolError::InvalidField {
            field,
            reason: "must be non-empty and within the length limit",
        });
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character))
    {
        return Err(MossProtocolError::InvalidField {
            field,
            reason: "contains unsupported characters",
        });
    }
    Ok(())
}

fn validate_revision(value: &str) -> Result<(), MossProtocolError> {
    if value.is_empty() || value.chars().count() > 128 {
        return Err(MossProtocolError::InvalidField {
            field: "model_revision",
            reason: "must be non-empty and at most 128 characters",
        });
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character))
    {
        return Err(MossProtocolError::InvalidField {
            field: "model_revision",
            reason: "contains unsupported characters",
        });
    }
    Ok(())
}

fn validate_window(start: u64, end: u64, max_span: u64) -> Result<(), MossProtocolError> {
    let Some(span) = end.checked_sub(start) else {
        return Err(MossProtocolError::InvalidField {
            field: "window",
            reason: "end must be after start",
        });
    };
    if span == 0 || span > max_span {
        return Err(MossProtocolError::InvalidField {
            field: "window",
            reason: "span is zero or exceeds the configured limit",
        });
    }
    Ok(())
}

fn validate_audio_path(path: &Path, root: &Path) -> Result<(), MossProtocolError> {
    if !path.is_absolute() || !root.is_absolute() {
        return Err(MossProtocolError::InvalidField {
            field: "audio_path",
            reason: "audio path and configured root must be absolute",
        });
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(MossProtocolError::InvalidField {
            field: "audio_path",
            reason: "dot path components are not allowed",
        });
    }
    if !path.starts_with(root) {
        return Err(MossProtocolError::PathOutsideRoot);
    }
    let supported_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("wav") || extension.eq_ignore_ascii_case("flac")
        });
    if !supported_extension {
        return Err(MossProtocolError::InvalidField {
            field: "audio_path",
            reason: "only wav and flac snapshots are accepted",
        });
    }
    Ok(())
}

fn validate_speaker_label(value: &str) -> Result<(), MossProtocolError> {
    validate_identifier("segments.speaker", value, 64)
}

fn validate_human_text(
    field: &'static str,
    value: &str,
    max_chars: usize,
    allow_newlines: bool,
) -> Result<(), MossProtocolError> {
    let length = value.chars().count();
    if value.trim().is_empty() || length > max_chars {
        return Err(MossProtocolError::InvalidField {
            field,
            reason: "must contain visible text within the length limit",
        });
    }
    if value.chars().any(|character| {
        character == '\0'
            || (character.is_control()
                && character != '\t'
                && !(allow_newlines && matches!(character, '\r' | '\n')))
    }) {
        return Err(MossProtocolError::InvalidField {
            field,
            reason: "contains unsupported control characters",
        });
    }
    Ok(())
}

fn serialize_line<T: Serialize>(
    value: &T,
    limits: MossProtocolLimits,
) -> Result<String, MossProtocolError> {
    let mut line =
        serde_json::to_string(value).map_err(|error| MossProtocolError::InvalidJson {
            line: error.line(),
            column: error.column(),
        })?;
    if line.len().saturating_add(1) > limits.max_jsonl_bytes {
        return Err(MossProtocolError::LineTooLong);
    }
    line.push('\n');
    Ok(line)
}

fn deserialize_line<T: for<'de> Deserialize<'de>>(
    input: &str,
    limits: MossProtocolLimits,
) -> Result<T, MossProtocolError> {
    if input.len() > limits.max_jsonl_bytes {
        return Err(MossProtocolError::LineTooLong);
    }
    let without_lf = input.strip_suffix('\n').unwrap_or(input);
    let without_eol = without_lf.strip_suffix('\r').unwrap_or(without_lf);
    if without_eol.contains('\r') || without_eol.contains('\n') {
        return Err(MossProtocolError::MultipleLines);
    }
    serde_json::from_str(without_eol).map_err(|error| MossProtocolError::InvalidJson {
        line: error.line(),
        column: error.column(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"<project-root>\app-data\temp\moss")
        } else {
            PathBuf::from("/workspace/meetily/app-data/temp/moss")
        }
    }

    fn request() -> MossWorkerRequest {
        MossWorkerRequest {
            schema: MOSS_WORKER_SCHEMA_VERSION,
            job: "job-0001".to_string(),
            session: "session-0001".to_string(),
            window_start_frame: 48_000,
            window_end_frame: 96_000,
            audio_path: root().join("window-0001.wav"),
            prompt: Some("技术评审会议；专有名词 Meetily".to_string()),
            model_revision: "0123456789abcdef".to_string(),
        }
    }

    fn response() -> MossWorkerResponse {
        MossWorkerResponse {
            schema: MOSS_WORKER_SCHEMA_VERSION,
            job: "job-0001".to_string(),
            session: "session-0001".to_string(),
            window_start_frame: 48_000,
            window_end_frame: 96_000,
            segments: vec![
                MossSegment {
                    start_frame: 50_000,
                    end_frame: 70_000,
                    speaker: "S01".to_string(),
                    text: "先确认接口边界。".to_string(),
                },
                MossSegment {
                    start_frame: 65_000,
                    end_frame: 80_000,
                    speaker: "S02".to_string(),
                    text: "我补充失败隔离方案。".to_string(),
                },
            ],
        }
    }

    #[test]
    fn valid_fixture_round_trips_as_one_jsonl_record() {
        let limits = MossProtocolLimits::default();
        let request = request();
        let line = request
            .to_json_line(&root(), limits)
            .expect("encode request");
        assert_eq!(line.matches('\n').count(), 1);
        let decoded =
            MossWorkerRequest::from_json_line(&line, &root(), limits).expect("decode request");
        assert_eq!(decoded, request);

        let response = response();
        let response_line = response
            .to_json_line(&request, limits)
            .expect("encode response");
        assert_eq!(
            MossWorkerResponse::from_json_line(&response_line, &request, limits)
                .expect("decode response"),
            response
        );

        let fixture = include_str!("testdata/moss_response_valid.jsonl");
        assert_eq!(
            MossWorkerResponse::from_json_line(fixture, &request, limits)
                .expect("validate repository fixture"),
            response
        );
    }

    #[test]
    fn debug_redacts_audio_prompt_and_transcript() {
        let request_debug = format!("{:?}", request());
        assert!(!request_debug.contains("专有名词"));
        assert!(!request_debug.contains("window-0001.wav"));
        let response_debug = format!("{:?}", response());
        assert!(!response_debug.contains("接口边界"));
    }

    #[test]
    fn rejects_escape_paths_and_unknown_json_fields() {
        let limits = MossProtocolLimits::default();
        let mut escaped = request();
        escaped.audio_path = root().join("..").join("secret.wav");
        assert!(matches!(
            escaped.validate(&root(), limits),
            Err(MossProtocolError::InvalidField {
                field: "audio_path",
                ..
            })
        ));

        let line = request().to_json_line(&root(), limits).unwrap();
        let line = line.trim_end().replace(
            "\"model_revision\":\"0123456789abcdef\"",
            "\"model_revision\":\"0123456789abcdef\",\"token\":\"must-not-pass\"",
        );
        assert!(matches!(
            MossWorkerRequest::from_json_line(&line, &root(), limits),
            Err(MossProtocolError::InvalidJson { .. })
        ));
    }

    #[test]
    fn allows_cross_speaker_overlap_but_rejects_self_overlap_and_unsorted_data() {
        let limits = MossProtocolLimits::default();
        let request = request();
        response()
            .validate_for_request(&request, limits)
            .expect("overlapped speech from different speakers is valid");

        let mut self_overlap = response();
        self_overlap.segments[1].speaker = "S01".to_string();
        assert_eq!(
            self_overlap.validate_for_request(&request, limits),
            Err(MossProtocolError::SameSpeakerOverlap)
        );

        let mut unsorted = response();
        unsorted.segments.swap(0, 1);
        assert_eq!(
            unsorted.validate_for_request(&request, limits),
            Err(MossProtocolError::UnsortedSegments)
        );
    }

    #[test]
    fn rejects_mismatched_job_and_out_of_window_segment() {
        let limits = MossProtocolLimits::default();
        let request = request();
        let mut mismatched = response();
        mismatched.job = "job-other".to_string();
        assert_eq!(
            mismatched.validate_for_request(&request, limits),
            Err(MossProtocolError::ResponseMismatch { field: "job" })
        );

        let mut outside = response();
        outside.segments[0].start_frame = 1;
        assert!(matches!(
            outside.validate_for_request(&request, limits),
            Err(MossProtocolError::InvalidField {
                field: "segments.range",
                ..
            })
        ));
    }
}
