//! Ordered access-unit bytes for an Annex B source, without a container clock.

use crate::retryable;
use crate::{
    avc::picture::AvcPictures,
    hevc,
    nal::{Codec, NalFraming, NalHeader, NalReader, NalUnit, ReaderLimits},
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

/// Bounds of a [`RawAnnexBReader`].
///
/// The default allows NAL units of 4 MiB, 4096 padding bytes, 32 million NAL
/// units in the stream, input windows of 64 KiB, and access units of 8 MiB and
/// 1024 NAL units.
#[derive(Clone, Copy, Debug)]
pub struct RawAnnexBLimits {
    /// Bounds of each NAL unit, of zero padding, of the stream's NAL unit count
    /// and of input windows. This reader grows a unit's buffer as its bytes
    /// arrive; it does not reserve `maximum_nal_bytes`.
    pub nal: ReaderLimits,
    /// The most source bytes one access unit may span, start codes and padding
    /// included. Must not be zero.
    pub maximum_access_unit_bytes: usize,
    /// The most NAL units one access unit may hold. Must not be zero.
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

/// One access unit of an Annex B elementary stream: where it lies in the source,
/// what it holds, and its NAL units, each with the buffer it was read into.
#[derive(Debug)]
pub struct RawAccessUnit {
    /// The source the access unit was read from.
    pub source: SourceId,
    /// The coding standard of the stream.
    pub codec: Codec,
    /// The source byte range of the access unit: from the zero padding or start
    /// code before its first NAL unit to the end of its last unit's trailing
    /// zeros.
    pub range: Range<u64>,
    /// At least one primary AVC slice or recognized base-layer HEVC slice.
    pub has_vcl: bool,
    /// Coded IDR or IRAP indication, before any decoder or seek policy.
    pub random_access: bool,
    nals: BudgetVec<NalUnit>,
}

impl RawAccessUnit {
    /// The access unit's NAL units in source order, parameter sets, SEI and
    /// delimiters included.
    pub fn nals(&self) -> &[NalUnit] {
        self.nals.as_slice()
    }

    /// The bytes of [`range`](Self::range) in order, as slices: all source
    /// bytes, including the original zero padding and prefix widths.
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

fn hevc_role(nal: &NalUnit, reader: &NalReader) -> Result<Role> {
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
    let vcl = layer_id == 0 && hevc::is_slice(nal_unit_type);
    let first_slice = vcl && nal.bytes().get(2).ok_or(Error::Truncated)? & 0x80 != 0;
    // FFmpeg's parser looks at a NAL unit once a byte after its two-byte
    // header is in, so a header-only unit that ends the stream begins nothing.
    let leading = layer_id == 0
        && hevc::begins_access_unit(nal_unit_type)
        && (nal.bytes().len() > 2 || nal.facts().trailing_zero_bytes > 0 || !reader.ended());
    Ok(Role {
        vcl,
        begins: first_slice || leading,
        random_access: first_slice && matches!(nal_unit_type, hevc::BLA_W_LP..=hevc::CRA),
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

/// Groups the NAL units of an Annex B elementary stream into access units. It
/// adds no timestamps.
///
/// Once an access unit holds a slice, it ends before the next NAL unit that
/// begins another one. For H.264 that is a unit of type 6 to 9 or 14 to 18
/// (7.4.1.2.3) and the first slice of another primary coded picture (7.4.1.2.4),
/// told apart with the parameter sets the stream carries. For H.265 it is a
/// base-layer unit for which [`hevc::begins_access_unit`] holds and a
/// base-layer slice with `first_slice_segment_in_pic_flag` set (7.4.2.4.4); a
/// unit of only its two header bytes that ends the stream begins nothing, as in
/// FFmpeg's parser. Every byte of the source belongs to one access unit, zero
/// padding included.
///
/// The reader is driven like a [`NalReader`]: [`step`](Self::step) asks for
/// input, and [`provide`](Self::provide), [`finish`](Self::finish) or
/// [`fail`](Self::fail) answers.
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

impl std::fmt::Debug for RawAnnexBReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RawAnnexBReader")
            .field("codec", &self.codec)
            .field("limits", &self.limits)
            .field("position", &self.nal.position())
            .field("published_bytes", &self.unit_start)
            .field("pending_nals", &self.unit.len())
            .field("end", &self.end)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl RawAnnexBReader {
    /// A reader at offset zero of `source`, charged to `pool`.
    /// The caller provides the codec from a trusted route or separate probe.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when a limit other than `maximum_padding_bytes` is
    /// zero.
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

    /// Answers the current request with `window`, whose bytes the reader
    /// shares. An empty window is no answer yet; only
    /// [`finish`](Self::finish) ends the source.
    ///
    /// # Errors
    ///
    /// A window that does not answer the current request is refused and changes
    /// nothing: another token, source, pool or offset, or more bytes than the
    /// request allows. After a final error of [`step`](Self::step), that error.
    pub fn provide(&mut self, window: &InputWindow) -> Result<()> {
        self.healthy()?;
        self.nal.provide(window)
    }

    /// [`provide`](Self::provide) for a window the caller gives away.
    ///
    /// # Errors
    ///
    /// As [`provide`](Self::provide); a refused window is dropped.
    pub fn provide_owned(&mut self, window: InputWindow) -> Result<()> {
        self.healthy()?;
        self.nal.provide_owned(window)
    }

    /// Start a fresh reader at an independently established access-unit boundary
    /// in the same elementary coordinate system. `preceding_nals` preserves the
    /// original NAL-count limit. No parameter sets or picture history are restored:
    /// the resumed bytes must supply all setup needed for framing and decoding.
    /// Refuses a reader which has consumed input or a count at its limit.
    #[doc(hidden)]
    pub fn resume_at(&mut self, offset: u64, preceding_nals: usize) -> Result<()> {
        self.healthy()?;
        if self.next_offset != 0 || self.pending.is_some() || !self.unit.is_empty() {
            return Err(Error::SourceMismatch);
        }
        self.nal.resume_annex_b(offset, preceding_nals)?;
        self.unit_start = offset;
        self.next_offset = offset;
        Ok(())
    }

    /// Consume a borrowed slice that fits wholly inside the current NAL
    /// payload. If the reader needs to inspect a possible boundary, the caller
    /// can provide the same slice as a regular input window.
    ///
    /// `Ok(true)` when `bytes` was taken as the answer to `request`. `Ok(false)`,
    /// with no byte taken, when the reader is not in the middle of a NAL unit,
    /// when `bytes` is not 32 to 4096 bytes long, ends in a zero byte or holds
    /// two zero bytes in a row, and when the work left or the unit's buffer
    /// cannot take it.
    ///
    /// # Errors
    ///
    /// A `request` other than the current one is refused, as by
    /// [`provide`](Self::provide); [`Error::Cancelled`] when `cancel` is set.
    pub fn append_payload(
        &mut self,
        request: &InputRequest,
        bytes: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        self.healthy()?;
        if self.pending.is_some() || self.end.is_some() {
            return Ok(false);
        }
        self.nal.append_annex_payload(request, bytes, work, cancel)
    }

    /// Answers the current request with the end of the source, which lies after
    /// the bytes already provided.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`], here and from every later call, when the source
    /// ends before its declared length. A `source` or `token` other than the
    /// current one is refused and changes nothing.
    pub fn finish(&mut self, source: SourceId, token: InputToken) -> Result<()> {
        self.healthy()?;
        let result = self.nal.finish(source, token);
        if matches!(result, Err(Error::Truncated)) {
            self.stop(Error::Truncated);
        }
        result
    }

    /// Answers the current request with a failure of the source. Every later
    /// call returns [`Error::Transport`] or [`Error::Cancelled`], as `failure`
    /// says.
    ///
    /// # Errors
    ///
    /// A `source` or `token` other than the current one is refused and changes
    /// nothing.
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

    /// Reads towards the next access unit. `Ready` carries one; `NeedInput`
    /// carries the request to answer; `End` follows the last access unit of a
    /// finished source. Work is charged per source byte and per NAL unit.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for H.264 slice data partitioning, redundant
    /// coded pictures, NAL unit types 13 to 16 and 19 to 21, and an SPS
    /// `profile_idc` other than 44, 66, 77, 88, 100, 110, 122 and 244;
    /// [`Error::Invalid`] for an H.264 slice whose parameter sets the stream
    /// has not carried; the error of [`read_sps`](crate::avc::read_sps),
    /// [`read_pps`](crate::avc::read_pps) or
    /// [`read_slice_header`](crate::avc::read_slice_header) for an H.264
    /// parameter set or slice header it refuses (emulation prevention is read
    /// strictly); [`Error::Truncated`] for an H.265 slice of only its header;
    /// [`Error::Limit`] past an access-unit limit; the errors of
    /// [`NalReader::step`] for the framing. [`Error::Backpressure`] and
    /// [`Error::Allocation`] are retryable: the reader keeps its progress. Every
    /// other error is final and returned again by each later call.
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RawAccessUnit>> {
        self.healthy()?;
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !retryable(error)
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
                    None => hevc_role(&pending.nal, &self.nal)?,
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

    /// Mutable buffers and completed NALs held while requesting more input.
    ///
    /// Returns an error if input or an unprocessed NAL is buffered.
    #[doc(hidden)]
    pub fn input_wait_storage(&self) -> Result<(usize, &[NalUnit])> {
        if self.pending.is_some() {
            return Err(Error::Invalid("Annex B reader has an unprocessed NAL"));
        }
        let nal_capacity = self.nal.input_wait_buffer_capacity()?;
        let capacity = self
            .unit
            .capacity()
            .checked_mul(size_of::<NalUnit>())
            .and_then(|bytes| bytes.checked_add(nal_capacity))
            .ok_or(Error::Overflow)?;
        Ok((capacity, self.unit.as_slice()))
    }

    /// The request for the bytes the reader takes next, as
    /// [`step`](Self::step) returns it with `NeedInput`.
    pub fn request(&self) -> InputRequest {
        self.nal.request()
    }

    /// The coding standard given at construction.
    pub const fn codec(&self) -> Codec {
        self.codec
    }

    /// The pool input windows, NAL units and access units are charged to.
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
