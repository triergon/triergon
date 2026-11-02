//! Original bounded native palette storage, separate from observed colour policy.
//! Owns per-snapshot output; no cross-display native palette cache is introduced.
use super::{D32_PIN, D32_VIEW, NativePalette};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RgbaStorage {
    /// Renderer has established exact equality entry by entry; preserve the old
    /// shared ownership for complete encoded palettes without allocating a copy.
    AliasStudioUnrepaired,
    /// Native default/automatic values have their own charged immutable owner.
    OwnedNative,
}

#[derive(Debug)]
pub(super) struct NativePaletteBuffer {
    budget: Budget,
    storage: RgbaStorage,
    count: usize,
    at: usize,
    rgba: Option<BudgetVec<u8>>,
    frozen_rgba: Option<Bytes>,
    words: Option<BudgetVec<u32>>,
    finished: bool,
}

impl NativePaletteBuffer {
    pub(super) fn new(budget: &Budget, count: usize, storage: RgbaStorage) -> Result<Self> {
        if !matches!(count, 4 | 16 | 256) {
            return Err(Error::Invalid("DVB native palette size"));
        }
        // Reserve the complete bounded target before any entry is committed.
        // Failure drops each earlier reservation without touching source data.
        let mut words = BudgetVec::new(budget);
        words.try_reserve_exact(count)?;
        let rgba = if storage == RgbaStorage::OwnedNative {
            let mut bytes = BudgetVec::new(budget);
            bytes.try_reserve_exact(count.checked_mul(4).ok_or(Error::Overflow)?)?;
            Some(bytes)
        } else {
            None
        };
        Ok(Self {
            budget: budget.clone(),
            storage,
            count,
            at: 0,
            rgba,
            frozen_rgba: None,
            words: Some(words),
            finished: false,
        })
    }

    /// One fixed-size store after the enclosing renderer pays its actual entry
    /// work. Any scan/ranking policy has a separate paid, resumable cursor.
    /// `index` makes replay/skip after Yield or pressure an explicit error.
    pub(super) fn push_entry(
        &mut self,
        index: usize,
        native: [u8; 4],
        studio_unrepaired: [u8; 4],
    ) -> Result<()> {
        if self.finished || self.frozen_rgba.is_some() || index != self.at || index >= self.count {
            return Err(Error::Invalid("DVB native palette entry order"));
        }
        if self.storage == RgbaStorage::AliasStudioUnrepaired && native != studio_unrepaired {
            return Err(Error::Invalid("DVB native palette alias differs"));
        }
        let words = self
            .words
            .as_mut()
            .ok_or(Error::Invalid("DVB native words absent"))?;
        if words.len() != index || words.len() >= words.capacity() {
            return Err(Error::Invalid("DVB native words reservation"));
        }
        if let Some(rgba) = self.rgba.as_mut() {
            let expected = index.checked_mul(4).ok_or(Error::Overflow)?;
            let next = expected.checked_add(4).ok_or(Error::Overflow)?;
            if rgba.len() != expected || next > rgba.capacity() {
                return Err(Error::Invalid("DVB native RGBA reservation"));
            }
        } else if self.storage == RgbaStorage::OwnedNative {
            return Err(Error::Invalid("DVB native RGBA absent"));
        }
        // Both capacity checks precede mutation. These bounded core appends do
        // not allocate/grow after complete constructor reservation.
        if let Some(rgba) = self.rgba.as_mut() {
            rgba.try_extend_from_slice(&native)?;
        }
        let word = u32::from(native[3]) << 24
            | u32::from(native[0]) << 16
            | u32::from(native[1]) << 8
            | u32::from(native[2]);
        words.try_push(word).map_err(|failure| failure.error)?;
        self.at += 1;
        Ok(())
    }

    /// Freeze without copying. If the owner-control reservation is under
    /// pressure, retain both builders/position for an identical paid retry.
    /// Raw source/studio palettes remain separately owned and untouched.
    pub(super) fn try_finish(
        &mut self,
        studio_unrepaired: &Bytes,
        all_entries_explicit: bool,
    ) -> Result<NativePalette> {
        if self.finished || self.at != self.count {
            return Err(Error::Invalid("DVB native palette incomplete"));
        }
        if !studio_unrepaired.belongs_to(&self.budget)
            || studio_unrepaired.len() != self.count.checked_mul(4).ok_or(Error::Overflow)?
        {
            return Err(Error::Invalid("DVB native source palette boundary"));
        }
        if self.frozen_rgba.is_none() {
            match self.storage {
                RgbaStorage::AliasStudioUnrepaired => {
                    self.frozen_rgba = Some(studio_unrepaired.clone());
                }
                RgbaStorage::OwnedNative => {
                    let rgba = self
                        .rgba
                        .take()
                        .ok_or(Error::Invalid("DVB native RGBA absent"))?;
                    match rgba.freeze() {
                        Ok(bytes) => self.frozen_rgba = Some(bytes),
                        Err(failure) => {
                            self.rgba = Some(failure.buffer);
                            return Err(failure.error);
                        }
                    }
                }
            }
        }
        let words = self
            .words
            .take()
            .ok_or(Error::Invalid("DVB native words absent"))?;
        let rgba = self
            .frozen_rgba
            .take()
            .ok_or(Error::Invalid("DVB native RGBA absent"))?;
        self.finished = true;
        Ok(NativePalette {
            identifier: D32_VIEW,
            source_pin: D32_PIN,
            rgba,
            words,
            all_entries_explicit,
        })
    }
}
