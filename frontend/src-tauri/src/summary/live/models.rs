use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_TITLE_CHARS: usize = 512;
const MAX_BODY_CHARS: usize = 65_536;

/// Stable evidence scope. A recording session deliberately has no meeting ID
/// until the transcript save transaction creates one. Keeping the kind
/// explicit prevents a renderer-visible opaque session from masquerading as a
/// canonical meeting identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum LiveSummaryScope {
    Meeting(String),
    RecordingSession(String),
}

impl LiveSummaryScope {
    pub fn meeting(meeting_id: impl Into<String>) -> Self {
        Self::Meeting(meeting_id.into())
    }

    pub fn recording_session(scope_id: impl Into<String>) -> Self {
        Self::RecordingSession(scope_id.into())
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Meeting(value) | Self::RecordingSession(value) => value,
        }
    }

    pub fn meeting_id(&self) -> Option<&str> {
        match self {
            Self::Meeting(value) => Some(value),
            Self::RecordingSession(_) => None,
        }
    }

    pub fn recording_session_id(&self) -> Option<&str> {
        match self {
            Self::Meeting(_) => None,
            Self::RecordingSession(value) => Some(value),
        }
    }

    pub fn validate(&self) -> Result<(), LiveSummaryContractError> {
        match self {
            Self::Meeting(value) => validate_identifier("scope.meeting_id", value),
            Self::RecordingSession(value) => {
                validate_identifier("scope.recording_session_id", value)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryItemKind {
    Topic,
    Decision,
    ActionItem,
    Risk,
    OpenQuestion,
}

impl SummaryItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Topic => "topic",
            Self::Decision => "decision",
            Self::ActionItem => "action_item",
            Self::Risk => "risk",
            Self::OpenQuestion => "open_question",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, LiveSummaryContractError> {
        match value {
            "topic" => Ok(Self::Topic),
            "decision" => Ok(Self::Decision),
            "action_item" => Ok(Self::ActionItem),
            "risk" => Ok(Self::Risk),
            "open_question" => Ok(Self::OpenQuestion),
            _ => Err(LiveSummaryContractError::InvalidField {
                field: "item.kind",
                reason: "is not a supported summary item kind",
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryItemStatus {
    Active,
    NeedsReview,
    Retracted,
}

impl SummaryItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::NeedsReview => "needs_review",
            Self::Retracted => "retracted",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, LiveSummaryContractError> {
        match value {
            "active" => Ok(Self::Active),
            "needs_review" => Ok(Self::NeedsReview),
            "retracted" => Ok(Self::Retracted),
            _ => Err(LiveSummaryContractError::InvalidField {
                field: "item.status",
                reason: "is not a supported summary item status",
            }),
        }
    }
}

/// Exact transcript version used as evidence for one summary item.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SummaryEvidence {
    pub scope: LiveSummaryScope,
    pub utterance_id: String,
    pub source_revision: u64,
    pub source_event_id: String,
    /// Lowercase SHA-256 of the exact UTF-8 transcript text bytes.
    pub source_text_hash: String,
}

impl SummaryEvidence {
    pub fn validate(&self) -> Result<(), LiveSummaryContractError> {
        self.scope.validate()?;
        validate_identifier("evidence.utterance_id", &self.utterance_id)?;
        validate_identifier("evidence.source_event_id", &self.source_event_id)?;
        validate_sha256("evidence.source_text_hash", &self.source_text_hash)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSummaryItem {
    /// Provider-independent stable identity. A later generation replaces an
    /// item by returning the same ID, rather than relying on display order.
    pub item_id: String,
    pub kind: SummaryItemKind,
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
    pub status: SummaryItemStatus,
    #[serde(default)]
    pub evidence: Vec<SummaryEvidence>,
}

impl LiveSummaryItem {
    pub fn validate(
        &self,
        expected_scope: &LiveSummaryScope,
    ) -> Result<(), LiveSummaryContractError> {
        validate_identifier("item.item_id", &self.item_id)?;
        validate_nonempty_text("item.title", &self.title, MAX_TITLE_CHARS)?;
        validate_nonempty_text("item.body", &self.body, MAX_BODY_CHARS)?;
        if let Some(owner) = self.owner.as_deref() {
            validate_nonempty_text("item.owner", owner, MAX_IDENTIFIER_BYTES)?;
        }
        if let Some(due_at) = self.due_at.as_deref() {
            validate_nonempty_text("item.due_at", due_at, 128)?;
        }

        let mut unique_evidence = HashSet::with_capacity(self.evidence.len());
        for evidence in &self.evidence {
            evidence.validate()?;
            if &evidence.scope != expected_scope {
                return Err(LiveSummaryContractError::ScopeMismatch);
            }
            if !unique_evidence.insert(evidence) {
                return Err(LiveSummaryContractError::DuplicateEvidence {
                    item_id: self.item_id.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSummaryRevisionType {
    Live,
    Final,
}

impl LiveSummaryRevisionType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Final => "final",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, LiveSummaryContractError> {
        match value {
            "live" => Ok(Self::Live),
            "final" => Ok(Self::Final),
            _ => Err(LiveSummaryContractError::InvalidField {
                field: "revision.revision_type",
                reason: "must be live or final",
            }),
        }
    }
}

/// A complete summary snapshot safe to persist. It contains structured output
/// only: no prompt, credential, transport body, or raw provider response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSummaryRevision {
    pub summary_revision_id: String,
    pub scope: LiveSummaryScope,
    pub revision: u64,
    pub generation: u64,
    pub transcript_cursor: u64,
    pub snapshot_hash: String,
    pub revision_type: LiveSummaryRevisionType,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub items: Vec<LiveSummaryItem>,
}

impl LiveSummaryRevision {
    pub fn validate(&self) -> Result<(), LiveSummaryContractError> {
        validate_identifier("revision.summary_revision_id", &self.summary_revision_id)?;
        self.scope.validate()?;
        if self.revision == 0 {
            return Err(LiveSummaryContractError::InvalidField {
                field: "revision.revision",
                reason: "must be at least one",
            });
        }
        if self.generation == 0 {
            return Err(LiveSummaryContractError::InvalidField {
                field: "revision.generation",
                reason: "must be at least one",
            });
        }
        validate_sha256("revision.snapshot_hash", &self.snapshot_hash)?;
        validate_nonempty_text("revision.provider", &self.provider, 128)?;
        if let Some(model) = self.model.as_deref() {
            validate_nonempty_text("revision.model", model, MAX_IDENTIFIER_BYTES)?;
        }
        validate_nonempty_text("revision.created_at", &self.created_at, 128)?;

        let mut item_ids = HashSet::with_capacity(self.items.len());
        for item in &self.items {
            item.validate(&self.scope)?;
            if !item_ids.insert(item.item_id.as_str()) {
                return Err(LiveSummaryContractError::DuplicateItemId {
                    item_id: item.item_id.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LiveSummaryContractError {
    #[error("invalid live summary field {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("live summary evidence does not match the actor or revision scope")]
    ScopeMismatch,
    #[error("duplicate live summary item ID: {item_id}")]
    DuplicateItemId { item_id: String },
    #[error("duplicate evidence in live summary item: {item_id}")]
    DuplicateEvidence { item_id: String },
    #[error("summary evidence is not present in the current transcript snapshot")]
    EvidenceDoesNotMatchSnapshot,
}

pub(crate) fn validate_identifier(
    field: &'static str,
    value: &str,
) -> Result<(), LiveSummaryContractError> {
    if value.trim().is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.contains('\0') {
        return Err(LiveSummaryContractError::InvalidField {
            field,
            reason: "must be a non-empty identifier of at most 256 bytes without NUL",
        });
    }
    Ok(())
}

fn validate_nonempty_text(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), LiveSummaryContractError> {
    if value.trim().is_empty() || value.chars().count() > max_chars || value.contains('\0') {
        return Err(LiveSummaryContractError::InvalidField {
            field,
            reason: "must be non-empty, within its length limit, and contain no NUL",
        });
    }
    Ok(())
}

pub(crate) fn validate_sha256(
    field: &'static str,
    value: &str,
) -> Result<(), LiveSummaryContractError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(LiveSummaryContractError::InvalidField {
            field,
            reason: "must be lowercase SHA-256 hex",
        });
    }
    Ok(())
}

/// Dependency-free SHA-256 keeps the event binding available in the core
/// without pulling a model/network crate into the recording process.
pub fn source_text_hash(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let padded_len = (bytes.len() + 9).div_ceil(64) * 64;
    let mut padded = Vec::with_capacity(padded_len);
    padded.extend_from_slice(bytes);
    padded.push(0x80);
    padded.resize(padded_len - 8, 0);
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for block in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut output = String::with_capacity(64);
    for word in state {
        use std::fmt::Write;
        write!(&mut output, "{word:08x}").expect("write SHA-256 hex into String");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_hash_is_exact_sha256() {
        assert_eq!(
            source_text_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            source_text_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(source_text_hash("你好"), source_text_hash("你好 "));
    }
}
