//! Work is one real record transfer or one publication, not a byte countdown.
//! Opaque source backing is already charged, validated and owned by core Bytes.
use super::{Attachment, Context, Header, Limits, Metadata, Recovery, Status};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, Result, SourceId, SourceLocation, WorkBudget,
};

pub struct Builder {
    pool: Budget,
    limits: Limits,
    header: Option<Header>,
    attachments: BudgetVec<Attachment>,
    metadata: BudgetVec<Metadata>,
    pending_attachment: Option<Attachment>,
    pending_metadata: Option<Metadata>,
    pending_bytes: usize,
    visible_bytes: usize,
    finished: bool,
    ended: bool,
    output: Option<Context>,
}

impl Builder {
    pub fn new(pool: &Budget, limits: Limits) -> Self {
        Self {
            pool: pool.clone(),
            limits,
            header: None,
            attachments: BudgetVec::new(pool),
            metadata: BudgetVec::new(pool),
            pending_attachment: None,
            pending_metadata: None,
            pending_bytes: 0,
            visible_bytes: 0,
            finished: false,
            ended: false,
            output: None,
        }
    }

    fn field(&self, bytes: &Bytes, maximum: usize) -> Result<usize> {
        if !bytes.belongs_to(&self.pool) {
            return Err(Error::ForeignPool);
        }
        limit(bytes.len(), maximum)?;
        Ok(bytes.len())
    }
    fn optional(&self, bytes: Option<&Bytes>, maximum: usize) -> Result<usize> {
        bytes.map_or(Ok(0), |bytes| self.field(bytes, maximum))
    }
    fn header_size(&self, header: &Header) -> Result<usize> {
        if header.identity.track_number == 0 || header.identity.track_uid == 0 {
            return Err(Error::Invalid("zero Matroska subtitle track identity"));
        }
        location(header.position, header.identity.source)?;
        let mut total = self.field(&header.codec_id, self.limits.max_label_bytes)?;
        add(
            &mut total,
            self.optional(header.codec_private.as_ref(), self.limits.max_config_bytes)?,
        )?;
        for field in [
            &header.name,
            &header.language,
            &header.language_bcp47,
            &header.charset_hint,
        ] {
            add(
                &mut total,
                self.optional(field.as_ref(), self.limits.max_label_bytes)?,
            )?;
        }
        limit(total, self.limits.max_visible_bytes)?;
        Ok(total)
    }
    fn accepting_record(&self) -> Result<SourceId> {
        if self.finished
            || self.output.is_some()
            || self.pending_attachment.is_some()
            || self.pending_metadata.is_some()
        {
            return Err(Error::Invalid("subtitle context is not accepting a record"));
        }
        self.header
            .as_ref()
            .map(|header| header.identity.source)
            .ok_or(Error::Invalid("subtitle context needs a header"))
    }
    fn additional(&self, bytes: usize) -> Result<()> {
        let total = self
            .visible_bytes
            .checked_add(bytes)
            .ok_or(Error::Overflow)?;
        limit(total, self.limits.max_visible_bytes)
    }

    /// Every rejection leaves the caller's owner in the original option.
    pub fn provide_header(&mut self, offered: &mut Option<Header>) -> Result<()> {
        if self.header.is_some() || self.output.is_some() {
            return Err(Error::Invalid("subtitle context header is busy"));
        }
        let header = offered
            .as_ref()
            .ok_or(Error::Invalid("empty subtitle context header offer"))?;
        let size = self.header_size(header)?;
        self.header = offered.take();
        self.visible_bytes = size;
        self.pending_bytes = 0;
        self.finished = false;
        self.ended = false;
        Ok(())
    }
    pub fn provide_attachment(&mut self, offered: &mut Option<Attachment>) -> Result<()> {
        let source = self.accepting_record()?;
        let attachment = offered
            .as_ref()
            .ok_or(Error::Invalid("empty subtitle attachment offer"))?;
        limit(
            self.attachments
                .len()
                .checked_add(1)
                .ok_or(Error::Overflow)?,
            self.limits.max_attachments,
        )?;
        if attachment.uid == 0 {
            return Err(Error::Invalid("zero Matroska attachment UID"));
        }
        location(attachment.position, source)?;
        let mut total = self.field(&attachment.name, self.limits.max_label_bytes)?;
        add(
            &mut total,
            self.field(&attachment.media_type, self.limits.max_label_bytes)?,
        )?;
        add(
            &mut total,
            self.optional(attachment.description.as_ref(), self.limits.max_label_bytes)?,
        )?;
        add(
            &mut total,
            self.field(&attachment.data, self.limits.max_attachment_bytes)?,
        )?;
        self.additional(total)?;
        self.pending_attachment = offered.take();
        self.pending_bytes = total;
        Ok(())
    }
    pub fn provide_metadata(&mut self, offered: &mut Option<Metadata>) -> Result<()> {
        let source = self.accepting_record()?;
        let metadata = offered
            .as_ref()
            .ok_or(Error::Invalid("empty subtitle metadata offer"))?;
        limit(
            self.metadata.len().checked_add(1).ok_or(Error::Overflow)?,
            self.limits.max_metadata,
        )?;
        location(metadata.position, source)?;
        let mut total = self.field(&metadata.key, self.limits.max_label_bytes)?;
        add(
            &mut total,
            self.field(&metadata.value, self.limits.max_label_bytes)?,
        )?;
        self.additional(total)?;
        self.pending_metadata = offered.take();
        self.pending_bytes = total;
        Ok(())
    }
    /// End this context entity explicitly. A source gap never calls this.
    pub fn finish(&mut self) -> Result<()> {
        if self.header.is_none() {
            return Err(Error::Invalid("subtitle context has no header"));
        }
        self.finished = true;
        Ok(())
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.output.is_some() {
            return Ok(Status::NeedOutput);
        }
        if self.ended {
            return Ok(Status::End);
        }
        if self.header.is_none() {
            return Ok(Status::NeedInput);
        }
        if self.pending_attachment.is_some() || self.pending_metadata.is_some() {
            if !work.try_charge(1) {
                return Ok(Status::Yield);
            }
            let total = self
                .visible_bytes
                .checked_add(self.pending_bytes)
                .ok_or(Error::Overflow)?;
            if let Some(attachment) = self.pending_attachment.take() {
                if let Err(failure) = self.attachments.try_push(attachment) {
                    self.pending_attachment = Some(failure.value);
                    return Err(failure.error);
                }
            } else if let Some(metadata) = self.pending_metadata.take()
                && let Err(failure) = self.metadata.try_push(metadata)
            {
                self.pending_metadata = Some(failure.value);
                return Err(failure.error);
            }
            self.visible_bytes = total;
            self.pending_bytes = 0;
        }
        if !self.finished {
            return Ok(Status::NeedInput);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(Status::Yield);
        }
        let header = self
            .header
            .take()
            .ok_or(Error::Invalid("missing subtitle context header"))?;
        self.output = Some(Context {
            header,
            attachments: std::mem::replace(&mut self.attachments, BudgetVec::new(&self.pool)),
            metadata: std::mem::replace(&mut self.metadata, BudgetVec::new(&self.pool)),
            visible_bytes: self.visible_bytes,
        });
        Ok(Status::Ready)
    }
    pub fn take_output(&mut self) -> Option<Context> {
        let output = self.output.take();
        if output.is_some() {
            self.ended = true;
        }
        output
    }
    pub fn recover(self) -> Recovery {
        Recovery {
            header: self.header,
            attachments: self.attachments,
            metadata: self.metadata,
            pending_attachment: self.pending_attachment,
            pending_metadata: self.pending_metadata,
            output: self.output,
        }
    }
}

fn limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(Error::Limit { requested, maximum })
    } else {
        Ok(())
    }
}
fn add(total: &mut usize, bytes: usize) -> Result<()> {
    *total = total.checked_add(bytes).ok_or(Error::Overflow)?;
    Ok(())
}
fn location(position: Option<SourceLocation>, source: SourceId) -> Result<()> {
    if position.is_some_and(|position| position.source != source) {
        Err(Error::SourceMismatch)
    } else {
        Ok(())
    }
}
