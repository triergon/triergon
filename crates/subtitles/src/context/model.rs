//! The records a container hands over after parsing. All byte fields are raw.
use crate::text::Input;
use triergon_core::{BudgetVec, Bytes, Error, Result, SourceId, SourceLocation};

/// Which track of which source reading a context belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The source the track is read from; every position must name it.
    pub source: SourceId,
    /// The reader generation the packets must carry.
    pub generation: u64,
    /// The configuration epoch the packets must carry; the container sets it.
    pub configuration_epoch: u64,
    /// The Matroska TrackNumber; nonzero.
    pub track_number: u64,
    /// The Matroska TrackUID; nonzero.
    pub track_uid: u64,
}

/// A subtitle track's header fields. `None` is an absent element, distinct
/// from a present empty one.
#[derive(Clone, Debug)]
pub struct Header {
    /// The track the header describes.
    pub identity: Identity,
    /// The CodecID bytes, such as `S_TEXT/ASS`.
    pub codec_id: Bytes,
    /// The CodecPrivate bytes.
    pub codec_private: Option<Bytes>,
    /// The track Name.
    pub name: Option<Bytes>,
    /// The Language element (ISO 639-2).
    pub language: Option<Bytes>,
    /// The LanguageBCP47 element.
    pub language_bcp47: Option<Bytes>,
    /// A caller-supplied hint; this does not invent a Matroska charset element.
    pub charset_hint: Option<Bytes>,
    /// Where the header was read.
    pub position: Option<SourceLocation>,
}

/// One attached file, such as a font.
#[derive(Clone, Debug)]
pub struct Attachment {
    /// The FileUID; nonzero.
    pub uid: u64,
    /// The FileName bytes.
    pub name: Bytes,
    /// The FileMediaType bytes.
    pub media_type: Bytes,
    /// The FileDescription bytes.
    pub description: Option<Bytes>,
    /// The FileData bytes.
    pub data: Bytes,
    /// Where the attachment was read.
    pub position: Option<SourceLocation>,
}

/// One metadata record, such as a title tag.
#[derive(Clone, Debug)]
pub struct Metadata {
    /// The key bytes, such as `title`.
    pub key: Bytes,
    /// The value bytes.
    pub value: Bytes,
    /// Where the record was read.
    pub position: Option<SourceLocation>,
}

/// Resource bounds of one context. Exceeding one is `Error::Limit`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Attachments per context.
    pub max_attachments: usize,
    /// Metadata records per context.
    pub max_metadata: usize,
    /// Bytes of [`Header::codec_private`].
    pub max_config_bytes: usize,
    /// Bytes of each other header field, attachment name, media type and
    /// description, and metadata key and value.
    pub max_label_bytes: usize,
    /// Bytes of each attachment's data.
    pub max_attachment_bytes: usize,
    /// Bytes of all fields together, as [`Context::visible_bytes`] counts them.
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

/// A finished context: the header with its attachments and metadata, which
/// no longer change. Field bytes share storage with the offered `Bytes`; the
/// record arrays stay charged to the builder's pool.
#[derive(Debug)]
pub struct Context {
    pub(crate) header: Header,
    pub(crate) attachments: BudgetVec<Attachment>,
    pub(crate) metadata: BudgetVec<Metadata>,
    pub(crate) visible_bytes: usize,
}
impl Context {
    /// The track header.
    pub fn header(&self) -> &Header {
        &self.header
    }
    /// The attachments, in the order provided.
    pub fn attachments(&self) -> &[Attachment] {
        self.attachments.as_slice()
    }
    /// The metadata records, in the order provided.
    pub fn metadata(&self) -> &[Metadata] {
        self.metadata.as_slice()
    }
    /// Logical lengths, counting each offered field. This is not live heap.
    pub fn visible_bytes(&self) -> usize {
        self.visible_bytes
    }
    /// Checks that `input` is a packet of this context's track:
    /// `Error::SourceMismatch` for another source, `Error::StaleGeneration`
    /// for another generation, `Error::Invalid` for another configuration
    /// epoch or track. `text::Input` has no track, so the caller passes
    /// `track_number`.
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

/// Every owner a builder held, from
/// [`Builder::recover`](crate::context::Builder::recover); recovering
/// allocates nothing.
#[derive(Debug)]
pub struct Recovery {
    /// The header; `None` before one is provided or once it is published.
    pub header: Option<Header>,
    /// The attachments a poll committed, unless they were published.
    pub attachments: BudgetVec<Attachment>,
    /// The metadata records a poll committed, unless they were published.
    pub metadata: BudgetVec<Metadata>,
    /// An attachment provided and not yet committed.
    pub pending_attachment: Option<Attachment>,
    /// A metadata record provided and not yet committed.
    pub pending_metadata: Option<Metadata>,
    /// A published context that was not taken.
    pub output: Option<Context>,
}

/// What a [`Builder::poll`](crate::context::Builder::poll) call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The builder waits for a header, a record or `finish`.
    NeedInput,
    /// The work budget ran out; poll again.
    Yield,
    /// The context is published; take it with `take_output`.
    Ready,
    /// The published context has not been taken.
    NeedOutput,
    /// The context was taken; a new header starts another.
    End,
}
