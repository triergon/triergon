//! Group complete E-AC-3 syncframes in a single private audio stream.
use crate::Pes;
use crate::reader::model::{DependentSyncframe, Shared};
use crate::reader::{Ac3AccessUnit, ReaderLimits, Track};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputRequest, InputWindow, Result, SourceCapabilities,
    SourceId, SourceLocation, Step, WorkBudget,
};
use triergon_framing::{
    Ac3Frame, Ac3FrameDetails, Ac3FrameFacts, Ac3FrameLimits, Ac3FrameReader, Ac3StreamSelection,
};

// A/52 Annex E §2.3.1 permits eight sequential dependent substreams
// immediately following their independent substream.
const MAX_DEPENDENTS: usize = 8;
// The 11-bit frame size gives the number of 16-bit words minus one.
const MAX_SYNCFRAME_BYTES: usize = 2 * (1 << 11);

pub(super) struct Reader {
    framer: Ac3FrameReader,
    pool: Budget,
    direct_carriage: bool,
    maximum_group_bytes: usize,
    maximum_incomplete_bytes: usize,
    group: Option<Group>,
    pending: Option<Ac3Frame>,
    sealed: bool,
    end: Option<SourceLocation>,
}

impl Reader {
    pub(super) fn new(
        pool: &Budget,
        track: &Track,
        limits: ReaderLimits,
        source: SourceId,
        caps: SourceCapabilities,
    ) -> Result<Self> {
        // EN 300 468 Annex D.5 signals additional independent services
        // separately. The reader admits the primary independent service.
        for descriptor in track.stream().descriptors() {
            let descriptor = descriptor?;
            if track.stream().stream_type == 0x06
                && descriptor.tag == 0x7a
                && descriptor
                    .dvb_eac3()?
                    .substreams
                    .iter()
                    .any(Option::is_some)
            {
                return Err(Error::Unsupported(
                    "DVB E-AC-3 additional independent services",
                ));
            }
        }
        let syncframe_bytes = limits.maximum_audio_frame_bytes.min(MAX_SYNCFRAME_BYTES);
        let maximum_group_bytes = syncframe_bytes
            .checked_mul(1 + MAX_DEPENDENTS)
            .ok_or(Error::Overflow)?;
        // A partial lookahead syncframe may follow the entire pending group.
        let maximum_incomplete_bytes = maximum_group_bytes
            .checked_add(syncframe_bytes)
            .ok_or(Error::Overflow)?;
        let framer = Ac3FrameReader::new(
            pool,
            source,
            caps,
            Ac3StreamSelection::Eac3Only,
            Ac3FrameLimits {
                maximum_frame_bytes: syncframe_bytes,
                // The transport reader counts emitted units. Allow a group beyond its cap and
                // one more primary to prove that group's extent.
                maximum_frames: limits
                    .maximum_access_units
                    .saturating_add(1)
                    .saturating_mul(1 + MAX_DEPENDENTS)
                    .saturating_add(1),
                maximum_input_window: limits.elementary_input_window,
            },
        )?;
        Ok(Self {
            framer,
            pool: pool.clone(),
            direct_carriage: track.stream().stream_type == 0x87,
            maximum_group_bytes,
            maximum_incomplete_bytes,
            group: None,
            pending: None,
            sealed: false,
            end: None,
        })
    }

    pub(super) fn maximum_incomplete_bytes(&self) -> usize {
        self.maximum_incomplete_bytes
    }

    pub(super) fn provide(&mut self, window: InputWindow) -> Result<()> {
        self.framer.provide_owned(window)
    }

    pub(super) fn provide_complete(
        &mut self,
        request: &InputRequest,
        pes: &Pes,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        debug_assert!(self.pending.is_none() && !self.sealed);
        match self.framer.read_frame_parts(
            request,
            pes.payload_len,
            pes.spans().iter().map(AsRef::as_ref),
            work,
            cancel,
        ) {
            Ok(Some(frame)) => {
                self.pending = Some(frame);
                Ok(true)
            }
            Ok(None)
            | Err(Error::Backpressure { .. } | Error::Allocation | Error::Limit { .. }) => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn finish(&mut self, request: InputRequest) -> Result<()> {
        self.framer.finish(request.source, request.token)
    }

    pub(super) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<Ac3AccessUnit>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(Step::Yield);
            }
            if self.sealed {
                let lookahead = self.pending.as_ref().map(Ac3Frame::bytes);
                let group = self.group.as_mut().expect("sealed E-AC-3 group");
                let Some(unit) = group.materialize(&self.pool, lookahead, work, cancel)? else {
                    return Ok(Step::Yield);
                };
                self.group = None;
                self.sealed = false;
                return Ok(Step::Ready(unit));
            }
            if let Some(location) = self.end {
                return Ok(Step::End(location));
            }
            if self.pending.is_none() {
                match self.framer.step(work, cancel) {
                    Ok(Step::Ready(frame)) => self.pending = Some(frame),
                    Ok(Step::NeedInput(request)) => return Ok(Step::NeedInput(request)),
                    Ok(Step::Yield) => return Ok(Step::Yield),
                    Ok(Step::End(location)) => {
                        self.end = Some(location);
                        self.sealed = self.group.is_some();
                        continue;
                    }
                    Ok(Step::Backpressure(never)) => match never {},
                    Err(error @ Error::Backpressure { requested, .. }) => {
                        if let Some(group) = &self.group {
                            require(&self.pool, group.storage()?, requested)?;
                        }
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                }
                // A completed frame remains staged if its framer used the grant.
                continue;
            }
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            let frame = self.pending.as_ref().expect("staged E-AC-3 syncframe");
            let facts = frame.facts();
            if let Some(group) = &self.group
                && group.end()? != facts.offset
            {
                return Err(Error::Invalid("DVB E-AC-3 source extent"));
            }
            match facts.header.details {
                Ac3FrameDetails::Eac3 {
                    stream_type: stream_type @ (0 | 2),
                    substream_id: 0,
                    ..
                } => {
                    if self.group.is_some() {
                        self.sealed = true;
                    } else {
                        // A/52 Annex G §3.3 requires six blocks per access unit.
                        if self.direct_carriage && (facts.header.blocks != 6 || stream_type != 0) {
                            return Err(Error::Unsupported(
                                "ATSC E-AC-3 requires a six-block independent syncframe",
                            ));
                        }
                        let frame = self.pending.take().expect("E-AC-3 primary");
                        self.group = Some(Group::new(frame));
                    }
                }
                Ac3FrameDetails::Eac3 {
                    stream_type: 1,
                    substream_id,
                    ..
                } => {
                    let group = self
                        .group
                        .as_mut()
                        .ok_or(Error::Invalid("DVB E-AC-3 dependent without primary"))?;
                    group.append(
                        &self.pool,
                        &mut self.pending,
                        substream_id,
                        self.maximum_group_bytes,
                    )?;
                }
                Ac3FrameDetails::Eac3 {
                    stream_type: 0 | 2, ..
                } => {
                    return Err(Error::Unsupported(
                        "DVB E-AC-3 additional independent service",
                    ));
                }
                _ => return Err(Error::Invalid("DVB E-AC-3 syncframe type")),
            }
        }
    }
}

struct Group {
    first: Ac3FrameFacts,
    primary: Option<Bytes>,
    // Create the table and its pool handle when the first dependent is admitted.
    dependents: Option<BudgetVec<DependentSyncframe>>,
    length: usize,
    joined: Option<BudgetVec<u8>>,
    frame_index: usize,
    copied: usize,
}

impl Group {
    fn new(frame: Ac3Frame) -> Self {
        Self {
            first: frame.facts(),
            length: frame.bytes().len(),
            primary: Some(frame.into_bytes()),
            dependents: None,
            joined: None,
            frame_index: 0,
            copied: 0,
        }
    }

    fn end(&self) -> Result<u64> {
        self.first
            .offset
            .checked_add(u64::try_from(self.length).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)
    }

    fn storage(&self) -> Result<usize> {
        let capacity = self
            .dependents
            .as_ref()
            .map_or(Ok(0), super::table_storage)?
            .checked_add(self.joined.as_ref().map_or(0, BudgetVec::capacity))
            .ok_or(Error::Overflow)?;
        self.primary
            .iter()
            .chain(
                self.dependents
                    .iter()
                    .flat_map(BudgetVec::as_slice)
                    .filter_map(|frame| frame.pending_bytes.as_ref()),
            )
            .try_fold(capacity, |total, bytes| {
                total
                    .checked_add(bytes.retained_storage_bytes())
                    .ok_or(Error::Overflow)
            })
    }

    fn append(
        &mut self,
        pool: &Budget,
        pending: &mut Option<Ac3Frame>,
        substream_id: u8,
        maximum_bytes: usize,
    ) -> Result<()> {
        let frame = pending.as_ref().expect("staged E-AC-3 dependent");
        let first = self.first.header;
        let facts = frame.facts();
        let header = facts.header;
        let count = self.dependents.as_ref().map_or(0, BudgetVec::len);
        if !matches!(first.details, Ac3FrameDetails::Eac3 { stream_type: 0, .. })
            || usize::from(substream_id) != count
            || first.sample_rate_hz != header.sample_rate_hz
            || first.blocks != header.blocks
        {
            return Err(Error::Invalid("DVB E-AC-3 dependent substream sequence"));
        }
        let length = self
            .length
            .checked_add(frame.bytes().len())
            .ok_or(Error::Overflow)?;
        if length > maximum_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: maximum_bytes,
            });
        }
        // The wire ID bounds the table to eight entries. Growth temporarily
        // retains both capacities, as well as the staged syncframe.
        let allocation = if self
            .dependents
            .as_ref()
            .is_none_or(|frames| count == frames.capacity())
        {
            (count + 1) * size_of::<DependentSyncframe>()
        } else {
            0
        };
        require(
            pool,
            self.storage()?,
            frame
                .bytes()
                .retained_storage_bytes()
                .checked_add(allocation)
                .ok_or(Error::Overflow)?,
        )?;
        // Reserve before moving the staged syncframe so a failed append can retry.
        let make = |_| DependentSyncframe {
            facts,
            pending_bytes: Some(
                pending
                    .take()
                    .expect("staged E-AC-3 dependent")
                    .into_bytes(),
            ),
        };
        if let Some(dependents) = &mut self.dependents {
            dependents.try_extend_with(1, make)?;
        } else {
            let mut dependents = BudgetVec::new(pool);
            dependents.try_extend_with(1, make)?;
            self.dependents = Some(dependents);
        }
        self.length = length;
        Ok(())
    }

    fn materialize(
        &mut self,
        pool: &Budget,
        lookahead: Option<&Bytes>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<Ac3AccessUnit>> {
        if self.dependents.is_none() {
            return Ok(work.try_charge(1).then(|| Ac3AccessUnit {
                first: self.first,
                bytes: self.primary.take().expect("E-AC-3 primary"),
                dependents: None,
            }));
        }
        let lookahead = lookahead.map_or(0, Bytes::retained_storage_bytes);
        if self.joined.is_none() {
            if !work.try_charge(1) {
                return Ok(None);
            }
            require(
                pool,
                self.storage()?
                    .checked_add(lookahead)
                    .ok_or(Error::Overflow)?,
                self.length,
            )?;
            let mut joined = BudgetVec::new(pool);
            joined.try_reserve_exact(self.length)?;
            self.joined = Some(joined);
        }
        let dependents = self.dependents.as_mut().expect("E-AC-3 dependent table");
        while self.frame_index <= dependents.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let source = if self.frame_index == 0 {
                &mut self.primary
            } else {
                &mut dependents.as_mut_slice()[self.frame_index - 1].pending_bytes
            };
            let bytes = source.as_ref().expect("pending E-AC-3 syncframe bytes");
            let count = work.take((bytes.len() - self.copied).min(4096) as u64) as usize;
            if count == 0 {
                return Ok(None);
            }
            let end = self.copied + count;
            self.joined
                .as_mut()
                .expect("E-AC-3 group buffer")
                .try_extend_from_slice(&bytes[self.copied..end])?;
            self.copied = end;
            if end == bytes.len() {
                *source = None;
                self.frame_index += 1;
                self.copied = 0;
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        let metadata = Shared::<BudgetVec<DependentSyncframe>>::allocation_bytes()?;
        let overhead = metadata
            .checked_add(Bytes::owner_overhead_bytes())
            .ok_or(Error::Overflow)?;
        require(
            pool,
            self.storage()?
                .checked_add(lookahead)
                .ok_or(Error::Overflow)?,
            overhead,
        )?;
        let charge = pool.reserve(metadata)?;
        match self.joined.take().expect("joined E-AC-3 group").freeze() {
            Ok(bytes) => {
                let frames = self.dependents.take().expect("E-AC-3 dependent table");
                Ok(Some(Ac3AccessUnit {
                    first: self.first,
                    bytes,
                    dependents: Some(Shared::new(frames, charge)),
                }))
            }
            Err(error) => {
                self.joined = Some(error.buffer);
                Err(error.error)
            }
        }
    }
}

// The local transition needs both charges. Other pool charges can still cause
// backpressure and may be released by the caller.
fn require(pool: &Budget, retained: usize, additional: usize) -> Result<()> {
    let requested = Budget::control_storage_bytes()
        .checked_add(retained)
        .and_then(|n| n.checked_add(additional))
        .ok_or(Error::Overflow)?;
    if requested > pool.limit() {
        return Err(Error::Limit {
            requested,
            maximum: pool.limit(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_framing::probe_ac3_header;

    const FRAME_BYTES: usize = 32;
    const GROUP_BYTES: usize = 2 * FRAME_BYTES;

    fn syncframe(dependent: bool) -> [u8; FRAME_BYTES] {
        let mut frame = [0; FRAME_BYTES];
        frame[..8].copy_from_slice(&[0x0b, 0x77, 0, 15, 0xb5, 0x87, 0xc0, 0]);
        if dependent {
            frame[2] = 0x40;
        }
        frame
    }

    fn group(pool: &Budget) -> Group {
        let frame = syncframe(false);
        let first = Ac3FrameFacts {
            source: SourceId(0),
            generation: 1,
            offset: 0,
            header: probe_ac3_header(&frame).unwrap(),
        };
        let primary = Bytes::copy_from_slice(pool, &frame).unwrap();
        let frame = syncframe(true);
        let mut dependents = BudgetVec::new(pool);
        dependents.try_reserve_exact(1).unwrap();
        dependents
            .try_push(DependentSyncframe {
                facts: Ac3FrameFacts {
                    offset: FRAME_BYTES as u64,
                    header: probe_ac3_header(&frame).unwrap(),
                    ..first
                },
                pending_bytes: Some(Bytes::copy_from_slice(pool, &frame).unwrap()),
            })
            .unwrap();
        Group {
            first,
            primary: Some(primary),
            dependents: Some(dependents),
            length: GROUP_BYTES,
            joined: None,
            frame_index: 0,
            copied: 0,
        }
    }

    #[test]
    fn first_dependent_survives_table_reservation_backpressure() {
        let pool = Budget::new(4096).unwrap();
        let mut framer = Ac3FrameReader::new(
            &pool,
            SourceId(0),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            Ac3StreamSelection::Eac3Only,
            Ac3FrameLimits {
                maximum_frame_bytes: FRAME_BYTES,
                maximum_frames: 2,
                maximum_input_window: GROUP_BYTES,
            },
        )
        .unwrap();
        let input = [syncframe(false), syncframe(true)].concat();
        let request = framer.request();
        let bytes = Bytes::copy_from_slice(&pool, &input).unwrap();
        framer
            .provide_owned(
                InputWindow::new(request.source, request.token, request.offset, bytes).unwrap(),
            )
            .unwrap();
        let mut work = WorkBudget::new(1024);
        let cancel = AtomicBool::new(false);
        let [first, dependent] = std::array::from_fn(|_| {
            let Step::Ready(frame) = framer.step(&mut work, &cancel).unwrap() else {
                panic!("complete syncframe");
            };
            frame
        });
        let mut group = Group::new(first);
        let facts = dependent.facts();
        let backing = dependent.bytes().as_ptr();
        let mut pending = Some(dependent);
        let before = pool.used();
        let pressure = pool.reserve(pool.available()).unwrap();
        assert_eq!(
            group
                .append(&pool, &mut pending, 0, GROUP_BYTES)
                .unwrap_err(),
            Error::Backpressure {
                requested: size_of::<DependentSyncframe>(),
                available: 0,
            }
        );
        assert_eq!(pending.as_ref().unwrap().facts(), facts);
        assert_eq!(pool.available(), 0);
        drop(pressure);
        assert_eq!(pool.used(), before);

        group.append(&pool, &mut pending, 0, GROUP_BYTES).unwrap();
        assert!(pending.is_none());
        let frame = &group.dependents.as_ref().unwrap().as_slice()[0];
        assert_eq!(frame.facts, facts);
        assert_eq!(frame.pending_bytes.as_ref().unwrap().as_ptr(), backing);
        assert_eq!(pool.used(), before + size_of::<DependentSyncframe>());
        drop((group, framer));
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn materialization_distinguishes_storage_limit_from_retryable_pressure() {
        let cancel = AtomicBool::new(false);
        let lookahead = FRAME_BYTES + Bytes::owner_overhead_bytes();
        let minimum = Budget::control_storage_bytes()
            + size_of::<DependentSyncframe>()
            + 3 * lookahead
            + GROUP_BYTES;
        let small = Budget::new(minimum - 1).unwrap();
        let staged_small = Bytes::copy_from_slice(&small, &[0; FRAME_BYTES]).unwrap();
        let mut short = group(&small);
        assert_eq!(
            short
                .materialize(
                    &small,
                    Some(&staged_small),
                    &mut WorkBudget::new(1),
                    &cancel
                )
                .unwrap_err(),
            Error::Limit {
                requested: minimum,
                maximum: minimum - 1
            }
        );
        assert!(short.joined.is_none());

        let pool = Budget::new(minimum).unwrap();
        let staged = Bytes::copy_from_slice(&pool, &[0; FRAME_BYTES]).unwrap();
        let mut group = group(&pool);
        let pressure = pool.reserve(1).unwrap();
        for _ in 0..2 {
            assert_eq!(
                group
                    .materialize(&pool, Some(&staged), &mut WorkBudget::new(1), &cancel)
                    .unwrap_err(),
                Error::Backpressure {
                    requested: GROUP_BYTES,
                    available: GROUP_BYTES - 1
                }
            );
            assert!(group.joined.is_none());
        }
        drop(pressure);
        assert!(
            group
                .materialize(&pool, Some(&staged), &mut WorkBudget::new(1), &cancel)
                .unwrap()
                .is_none()
        );
        let expected = [
            group.primary.as_ref().unwrap().as_ref(),
            group.dependents.as_ref().unwrap().as_slice()[0]
                .pending_bytes
                .as_ref()
                .unwrap()
                .as_ref(),
        ]
        .concat();
        assert!(
            group
                .materialize(
                    &pool,
                    Some(&staged),
                    &mut WorkBudget::new(GROUP_BYTES as u64),
                    &cancel,
                )
                .unwrap()
                .is_none()
        );
        assert!(group.primary.is_none());
        assert!(
            group.dependents.as_ref().unwrap().as_slice()[0]
                .pending_bytes
                .is_none()
        );
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            group
                .materialize(&pool, Some(&staged), &mut WorkBudget::new(0), &cancel)
                .unwrap_err(),
            Error::Cancelled
        );
        cancel.store(false, Ordering::Relaxed);
        let metadata = Shared::<BudgetVec<DependentSyncframe>>::allocation_bytes().unwrap();
        let pressure = pool.reserve(pool.available() - metadata).unwrap();
        assert_eq!(
            group
                .materialize(&pool, Some(&staged), &mut WorkBudget::new(1), &cancel)
                .unwrap_err(),
            Error::Backpressure {
                requested: Bytes::owner_overhead_bytes(),
                available: 0
            }
        );
        drop(pressure);
        let joined = group
            .materialize(&pool, Some(&staged), &mut WorkBudget::new(1), &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(joined.bytes().as_ref(), expected);
        drop((group, joined, staged));
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}
