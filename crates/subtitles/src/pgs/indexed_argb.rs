//! Owned indexed output and bounded ARGB writes. Source indices and
//! palette provenance remain immutable, charged owners; receiver words do not
//! establish source colour knownness. The materialized decoder stays default.
use super::display_model::PaletteSnapshot;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

mod sealed {
    use super::*;

    pub trait Storage: Sized {
        const MATERIALIZE: bool;
        fn finish(bytes: Option<Bytes>) -> Result<Self>;
    }

    impl Storage for Bytes {
        const MATERIALIZE: bool = true;

        fn finish(bytes: Option<Bytes>) -> Result<Self> {
            bytes.ok_or(Error::Invalid("PGS ARGB output absent"))
        }
    }

    impl Storage for IndexedArgb {
        const MATERIALIZE: bool = false;

        fn finish(bytes: Option<Bytes>) -> Result<Self> {
            if bytes.is_some() {
                return Err(Error::Invalid("PGS indexed output eagerly materialized"));
            }
            Ok(IndexedArgb)
        }
    }
}

/// Closed storage selection shared by the same source parser. Implementations
/// are limited to the existing materialized `Bytes` output and `IndexedArgb`.
pub trait ArgbStorage: sealed::Storage + std::fmt::Debug {}
impl ArgbStorage for Bytes {}
impl ArgbStorage for IndexedArgb {}

/// Indexed output retains the object's complete indices and palette. It has no
/// eager ARGB allocation and exposes no materialized-byte accessor.
#[derive(Debug)]
pub struct IndexedArgb;

/// Owned, immutable projection into canonical A,R,G,B bytes. Cloning retains
/// the actual source index and palette owners; it never clones a pixel plane.
#[derive(Clone, Debug)]
pub struct NativeArgbView {
    indices: Bytes,
    palette: PaletteSnapshot,
    suppressed: bool,
    byte_len: usize,
}

impl NativeArgbView {
    pub(crate) fn new(indices: Bytes, palette: PaletteSnapshot, suppressed: bool) -> Result<Self> {
        if !indices.belongs_to(&palette.0.budget) {
            return Err(Error::ForeignPool);
        }
        let byte_len = indices.len().checked_mul(4).ok_or(Error::Overflow)?;
        Ok(Self {
            indices,
            palette,
            suppressed,
            byte_len,
        })
    }

    pub fn len(&self) -> usize {
        self.byte_len
    }

    pub fn is_empty(&self) -> bool {
        self.byte_len == 0
    }

    pub fn indices(&self) -> &Bytes {
        &self.indices
    }

    pub fn palette(&self) -> &PaletteSnapshot {
        &self.palette
    }

    pub fn forced_only_suppressed(&self) -> bool {
        self.suppressed
    }

    /// Start a fresh bounded write operation. Each written byte costs one work
    /// unit. The target belongs to the caller and is never retained or resized.
    pub fn writer(self, maximum_work: usize) -> ArgbWriter {
        ArgbWriter {
            length: self.byte_len,
            view: Some(self),
            offset: 0,
            spent: 0,
            maximum_work,
            failed: None,
        }
    }

    fn write_span(&self, output: &mut [u8], offset: usize) {
        if self.suppressed {
            output.fill(0);
            return;
        }
        let mut pixel = offset / 4;
        let skip = offset % 4;
        let prefix = if skip == 0 {
            0
        } else {
            (4 - skip).min(output.len())
        };
        if prefix != 0 {
            let word = self.palette.native_argb(self.indices[pixel]).to_be_bytes();
            output[..prefix].copy_from_slice(&word[skip..skip + prefix]);
            pixel += 1;
        }
        let whole = (output.len() - prefix) / 4;
        let (full, tail) = output[prefix..].split_at_mut(whole * 4);
        let indices = &self.indices[pixel..pixel + whole];
        // Palette construction completes all 256 entries before publication.
        // Carry that invariant into this loop so each byte index is in range.
        let words: &[u32; 256] = self
            .palette
            .native_words()
            .try_into()
            .expect("published PGS palette has all 256 entries");
        let block_pixels = whole / 4 * 4;
        let (blocks, remainder) = full.split_at_mut(block_pixels * 4);
        // Four adjacent pixels per loop. All four indices are still read and
        // every funded channel is written directly into caller storage. The
        // fixed slices permit store/vector combining without an intermediate
        // plane, palette conversion, allocation or extra source-byte pass.
        for (bytes, index) in blocks.chunks_exact_mut(16).zip(indices.chunks_exact(4)) {
            bytes[..4].copy_from_slice(&words[usize::from(index[0])].to_be_bytes());
            bytes[4..8].copy_from_slice(&words[usize::from(index[1])].to_be_bytes());
            bytes[8..12].copy_from_slice(&words[usize::from(index[2])].to_be_bytes());
            bytes[12..16].copy_from_slice(&words[usize::from(index[3])].to_be_bytes());
        }
        for (bytes, index) in remainder.chunks_exact_mut(4).zip(&indices[block_pixels..]) {
            bytes.copy_from_slice(&words[usize::from(*index)].to_be_bytes());
        }
        if !tail.is_empty() {
            let word = self
                .palette
                .native_argb(self.indices[pixel + whole])
                .to_be_bytes();
            tail.copy_from_slice(&word[..tail.len()]);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgbWrite {
    /// Exactly this prefix of the supplied target was written this call.
    pub written: usize,
    pub complete: bool,
}

/// Resumable ARGB writer. Calls write at most 256 bytes after funding them;
/// zero work leaves the target and cursor untouched. Cancellation and work
/// overflow/limits are sticky and release the writer's retained source owners.
#[derive(Debug)]
pub struct ArgbWriter {
    view: Option<NativeArgbView>,
    length: usize,
    offset: usize,
    spent: usize,
    maximum_work: usize,
    failed: Option<Error>,
}

impl ArgbWriter {
    pub fn is_complete(&self) -> bool {
        self.failed.is_none() && self.offset == self.length
    }

    pub(super) fn check_budget(&self, budget: &Budget) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let view = self
            .view
            .as_ref()
            .ok_or(Error::Invalid("PGS ARGB writer owner absent"))?;
        if !view.indices.belongs_to(budget) {
            return Err(Error::ForeignPool);
        }
        Ok(())
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn work_spent(&self) -> usize {
        self.spent
    }

    fn poison(&mut self, error: Error) -> Error {
        self.view = None;
        self.failed = Some(error);
        error
    }

    pub fn write(
        &mut self,
        target: &mut [u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ArgbWrite> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.offset == self.length {
            return Ok(ArgbWrite {
                written: 0,
                complete: true,
            });
        }
        if target.is_empty() || work.remaining() == 0 {
            return Ok(ArgbWrite {
                written: 0,
                complete: false,
            });
        }
        let available = self.maximum_work.saturating_sub(self.spent);
        if available == 0 {
            let requested = match self.spent.checked_add(1) {
                Some(value) => value,
                None => return Err(self.poison(Error::Overflow)),
            };
            return Err(self.poison(Error::Limit {
                requested,
                maximum: self.maximum_work,
            }));
        }
        let count = target
            .len()
            .min(256)
            .min(self.length - self.offset)
            .min(usize::try_from(work.remaining()).unwrap_or(usize::MAX))
            .min(available);
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| self.poison(Error::Overflow))?;
        let spent = self
            .spent
            .checked_add(count)
            .ok_or_else(|| self.poison(Error::Overflow))?;
        let view = self
            .view
            .as_ref()
            .ok_or(Error::Invalid("PGS ARGB writer owner absent"))?;
        if !work.try_charge(u64::try_from(count).map_err(|_| Error::Overflow)?) {
            return Err(Error::Invalid("PGS ARGB writer funding"));
        }
        view.write_span(&mut target[..count], self.offset);
        self.offset = end;
        self.spent = spent;
        Ok(ArgbWrite {
            written: count,
            complete: end == self.length,
        })
    }
}
