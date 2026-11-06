//! Original context handoff, after container parsing. All byte fields are raw.
use crate::text::Input;
use triergon_core::{BudgetVec, Bytes, Error, Result, SourceId, SourceLocation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub source: SourceId,
    pub generation: u64,
    pub configuration_epoch: u64,
    pub track_number: u64,
    pub track_uid: u64,
}

#[derive(Clone, Debug)]
pub struct Header {
    pub identity: Identity,
    pub codec_id: Bytes,
    pub codec_private: Option<Bytes>,
    pub name: Option<Bytes>,
    pub language: Option<Bytes>,
    pub language_bcp47: Option<Bytes>,
    /// A caller-supplied hint; this does not invent a Matroska charset element.
    pub charset_hint: Option<Bytes>,
    pub position: Option<SourceLocation>,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub uid: u64,
    pub name: Bytes,
    pub media_type: Bytes,
    pub description: Option<Bytes>,
    pub data: Bytes,
    pub position: Option<SourceLocation>,
}

#[derive(Clone, Debug)]
pub struct Metadata {
    pub key: Bytes,
    pub value: Bytes,
    pub position: Option<SourceLocation>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_attachments: usize,
    pub max_metadata: usize,
    pub max_config_bytes: usize,
    pub max_label_bytes: usize,
    pub max_attachment_bytes: usize,
    pub max_visible_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_attachments: 128,
            max_metadata: 256,
            max_config_bytes: 1024 * 1024,
            max_label_bytes: 64 * 1024,
            max_attachment_bytes: 32 * 1024 * 1024,
            max_visible_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Immutable owner after the producer closes. Payload/configuration owners are
/// shared without copying, while both record arrays remain charged.
#[derive(Debug)]
pub struct Context {
    pub(crate) header: Header,
    pub(crate) attachments: BudgetVec<Attachment>,
    pub(crate) metadata: BudgetVec<Metadata>,
    pub(crate) visible_bytes: usize,
}
impl Context {
    pub fn header(&self) -> &Header {
        &self.header
    }
    pub fn attachments(&self) -> &[Attachment] {
        self.attachments.as_slice()
    }
    pub fn metadata(&self) -> &[Metadata] {
        self.metadata.as_slice()
    }
    /// Logical lengths, counting each offered field. This is not live heap.
    pub fn visible_bytes(&self) -> usize {
        self.visible_bytes
    }
    /// Integer identity checks only. The container owns epoch transitions and
    /// the caller supplies track identity because text::Input has no track ID.
    pub fn check_packet(&self, input: &Input, track_number: u64) -> Result<()> {
        let identity = self.header.identity;
        if input.source_id != identity.source {
            return Err(Error::SourceMismatch);
        }
        if input.generation != identity.generation {
            return Err(Error::StaleGeneration {
                expected: identity.generation,
                received: input.generation,
            });
        }
        if input.configuration_epoch != identity.configuration_epoch {
            return Err(Error::Invalid("subtitle context configuration epoch"));
        }
        if track_number != identity.track_number {
            return Err(Error::Invalid("subtitle context track"));
        }
        Ok(())
    }
}

/// Aborting consumes no allocation and preserves committed and pending owners.
#[derive(Debug)]
pub struct Recovery {
    pub header: Option<Header>,
    pub attachments: BudgetVec<Attachment>,
    pub metadata: BudgetVec<Metadata>,
    pub pending_attachment: Option<Attachment>,
    pub pending_metadata: Option<Metadata>,
    pub output: Option<Context>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    NeedInput,
    Yield,
    Ready,
    NeedOutput,
    End,
}
