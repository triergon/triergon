//! Ordered access-unit bytes for an Annex B source, without a container clock.

use crate::{
    Codec, NalFraming, NalHeader, NalReader, NalUnit, ReaderLimits, avc_picture::AvcPictures,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, BudgetVec, Error, InputRequest, InputToken, InputWindow, Result, SourceCapabilities,
    SourceFailure, SourceId, SourceLocation, Step, WorkBudget,
};

const ZEROS: [u8; 256] = [0; 256];
const START_3: &[u8] = &[0, 0, 1];
const START_4: &[u8] = &[0, 0, 0, 1];

#[derive(Clone, Copy, Debug)]
pub struct RawAnnexBLimits {
    pub nal: ReaderLimits,
    /// Exact original extent, including start codes and padding.
    pub maximum_access_unit_bytes: usize,
    pub maximum_access_unit_nals: usize,
}

impl Default for RawAnnexBLimits {
    fn default() -> Self {
        Self {
            nal: ReaderLimits {
                maximum_nal_bytes: 4 * 1024 * 1024,
                maximum_padding_bytes: 4096,
                maximum_nal_units: 32_000_000,
                maximum_input_window: 64 * 1024,
            },
            maximum_access_unit_bytes: 8 * 1024 * 1024,
            maximum_access_unit_nals: 1024,
        }
    }
}

/// Original source facts and a scatter view over the immutable NAL owners.
#[derive(Debug)]
pub struct RawAccessUnit {
    pub source: SourceId,
    pub codec: Codec,
    pub range: Range<u64>,
    /// At least one primary AVC slice or recognized base-layer HEVC slice.
    pub has_vcl: bool,
    /// Coded IDR or IRAP indication, before any decoder or seek policy.
    pub random_access: bool,
    nals: BudgetVec<NalUnit>,
}

impl RawAccessUnit {
    pub fn nals(&self) -> &[NalUnit] {
        self.nals.as_slice()
    }

    /// All source bytes, including the original zero padding and prefix widths.
    pub fn segments(&self) -> impl Iterator<Item = &[u8]> {
        self.nals().iter().flat_map(|nal| {
            let facts = nal.facts();
            let (leading, prefix) = match facts.framing {
                NalFraming::AnnexB {
                    leading_zero_bytes,
                    start_code_bytes,
                    ..
                } => (
                    leading_zero_bytes,
                    if start_code_bytes == 4 {
                        START_4
                    } else {
                        START_3
                    },
                ),
                NalFraming::LengthPrefixed { .. } => unreachable!("raw reader uses Annex B"),
            };
            zeros(leading)
                .chain([prefix, nal.bytes().as_ref()])
                .chain(zeros(facts.trailing_zero_bytes))
        })
    }
}

fn zeros<'a>(mut count: usize) -> impl Iterator<Item = &'a [u8]> {
    std::iter::from_fn(move || {
        let chunk = count.min(ZEROS.len());
        count -= chunk;
        (chunk != 0).then_some(&ZEROS[..chunk])
    })
}

#[derive(Clone, Copy)]
struct Role {
    vcl: bool,
    begins: bool,
    random_access: bool,
}

fn hevc_role(nal: &NalUnit) -> Result<Role> {
    let NalHeader::Hevc {
        nal_unit_type,
        layer_id,
        ..
    } = nal.facts().header
    else {
        return Err(Error::Invalid("raw HEVC NAL header"));
    };
    // H.265 7.4.2.4.4. Only base-layer NALs choose the access-unit
    // boundary. Every other NAL retains its source bytes in order.
    let vcl = layer_id == 0 && matches!(nal_unit_type, 0..=9 | 16..=21);
    let first_slice = vcl && nal.bytes().get(2).ok_or(Error::Truncated)? & 0x80 != 0;
    let leading = layer_id == 0 && matches!(nal_unit_type, 32..=35 | 39 | 41..=44 | 48..=55);
    Ok(Role {
        vcl,
        begins: first_slice || leading,
        random_access: first_slice && matches!(nal_unit_type, 16..=21),
    })
}

fn avc_role(pictures: &mut AvcPictures, nal: &NalUnit, has_vcl: bool) -> Result<Role> {
    let header = nal.facts().header;
    let kind = header.nal_unit_type();
    // H.264 7.4.1.2.3 distinguishes leading NALs from EOS/EOB suffixes.
    let leading = matches!(kind, 6..=9 | 14..=18);
    if (has_vcl && leading) || matches!(kind, 10 | 11) {
        pictures.reset_picture();
    }
    let fact = pictures.observe(nal.bytes().as_ref(), header)?;
    Ok(Role {
        vcl: fact.vcl,
        begins: leading || fact.new_picture,
        random_access: fact.random_access,
    })
}

struct PendingNal {
    nal: NalUnit,
    role: Option<Role>,
}

pub struct RawAnnexBReader {
    nal: NalReader,
    codec: Codec,
    avc: Option<AvcPictures>,
    limits: RawAnnexBLimits,
    unit: BudgetVec<NalUnit>,
    pending: Option<PendingNal>,
    unit_start: u64,
    next_offset: u64,
    has_vcl: bool,
    random_access: bool,
    end: Option<SourceLocation>,
    failure: Option<Error>,
}

impl RawAnnexBReader {
    /// The caller provides the codec from a trusted route or separate probe.
    pub fn new(
        pool: &Budget,
        codec: Codec,
        source: SourceId,
        caps: SourceCapabilities,
        limits: RawAnnexBLimits,
    ) -> Result<Self> {
        if limits.maximum_access_unit_bytes == 0 || limits.maximum_access_unit_nals == 0 {
            return Err(Error::Invalid("zero raw access-unit limit"));
        }
        Ok(Self {
            nal: NalReader::compact_annex_b(pool, codec, source, caps, limits.nal)?,
            codec,
            avc: (codec == Codec::Avc).then(AvcPictures::default),
            limits,
            unit: BudgetVec::new(pool),
            pending: None,
            unit_start: 0,
            next_offset: 0,
            has_vcl: false,
            random_access: false,
            end: None,
            failure: None,
        })
    }

    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.healthy()?;
        self.nal.provide(window)
    }

    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.healthy()?;
        self.nal.provide_owned(window)
    }

    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let result = self.nal.finish(source, token);
        if matches!(result, Err(Error::Truncated)) {
            self.stop(Error::Truncated);
        }
        result
    }

    pub fn fail(
        &mut self,
        source: SourceId,
        token: InputToken,
        failure: SourceFailure,
    ) -> Result<()> {
        self.healthy()?;
        self.nal.fail(source, token, failure)?;
        self.stop(match failure {
            SourceFailure::Transport => Error::Transport,
            SourceFailure::Cancelled => Error::Cancelled,
        });
        Ok(())
    }

    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RawAccessUnit>> {
        self.healthy()?;
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.stop(error);
        }
        result
    }

    fn healthy(&self) -> Result<()> {
        self.failure.map_or(Ok(()), Err)
    }

    fn stop(&mut self, error: Error) {
        self.failure = Some(error);
        self.nal.stop(error);
        self.pending = None;
        self.unit = BudgetVec::new(self.nal.budget());
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RawAccessUnit>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(location) = self.end {
                return Ok(if self.unit.is_empty() {
                    Step::End(location)
                } else {
                    Step::Ready(self.publish())
                });
            }
            if self.pending.is_none() {
                match self.nal.step(work, cancel)? {
                    Step::Ready(nal) => self.pending = Some(PendingNal { nal, role: None }),
                    Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                    Step::Yield => return Ok(Step::Yield),
                    Step::End(location) => {
                        if self.next_offset != location.offset {
                            return Err(Error::Invalid("raw Annex B final extent"));
                        }
                        self.end = Some(location);
                        continue;
                    }
                }
            }
            let pending = self
                .pending
                .as_mut()
                .ok_or(Error::Invalid("raw pending NAL"))?;
            let role = if let Some(role) = pending.role {
                role
            } else {
                if !work.try_charge(1) {
                    return Ok(Step::Yield);
                }
                let role = match &mut self.avc {
                    Some(pictures) => avc_role(pictures, &pending.nal, self.has_vcl)?,
                    None => hevc_role(&pending.nal)?,
                };
                pending.role = Some(role);
                role
            };
            if self.has_vcl && role.begins {
                return Ok(Step::Ready(self.publish()));
            }
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            self.append(role)?;
        }
    }

    fn append(&mut self, role: Role) -> Result<()> {
        let pending = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("raw pending NAL"))?;
        let facts = pending.nal.facts();
        let NalFraming::AnnexB {
            prefix_offset,
            leading_zero_bytes,
            ..
        } = facts.framing
        else {
            return Err(Error::Invalid("raw NAL framing"));
        };
        let start = prefix_offset
            .checked_sub(leading_zero_bytes as u64)
            .ok_or(Error::Overflow)?;
        if start != self.next_offset {
            return Err(Error::Invalid("raw Annex B discontinuous extent"));
        }
        let end = facts
            .payload_offset
            .checked_add(facts.payload_length as u64)
            .and_then(|end| end.checked_add(facts.trailing_zero_bytes as u64))
            .ok_or(Error::Overflow)?;
        let length = end.checked_sub(self.unit_start).ok_or(Error::Overflow)?;
        let extent = usize::try_from(length).map_err(|_| Error::Overflow)?;
        check_limit(extent, self.limits.maximum_access_unit_bytes)?;
        check_limit(self.unit.len() + 1, self.limits.maximum_access_unit_nals)?;

        let pending = self
            .pending
            .take()
            .ok_or(Error::Invalid("raw pending NAL"))?;
        if let Err(rejected) = self.unit.try_push(pending.nal) {
            self.pending = Some(PendingNal {
                nal: rejected.value,
                role: pending.role,
            });
            return Err(rejected.error);
        }
        self.next_offset = end;
        self.has_vcl |= role.vcl;
        self.random_access |= role.random_access;
        Ok(())
    }

    fn publish(&mut self) -> RawAccessUnit {
        let nals = std::mem::replace(&mut self.unit, BudgetVec::new(self.nal.budget()));
        let range = self.unit_start..self.next_offset;
        self.unit_start = self.next_offset;
        RawAccessUnit {
            source: self.nal.source(),
            codec: self.codec,
            range,
            has_vcl: std::mem::take(&mut self.has_vcl),
            random_access: std::mem::take(&mut self.random_access),
            nals,
        }
    }

    pub fn request(&self) -> InputRequest {
        self.nal.request()
    }

    pub const fn codec(&self) -> Codec {
        self.codec
    }

    pub fn budget(&self) -> &Budget {
        self.nal.budget()
    }

    /// Source extent already handed to the consumer.
    pub const fn published_bytes(&self) -> u64 {
        self.unit_start
    }
}

fn check_limit(requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        return Err(Error::Limit { requested, maximum });
    }
    Ok(())
}
