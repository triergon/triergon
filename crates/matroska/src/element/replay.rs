//! Seek-time replay on the envelope's one cursor: Cluster ranges and block replay
//! locations, internal range restore, the owned-prefix check of in-memory
//! Clusters, and bounded seek operations. Internal ranges keep the input
//! generation; only the explicit seek transition starts a new one.
use super::control::EnvelopeProgress;
use super::{
    CLUSTER, ClusterCheckpoint, EBML, ElementHeader, ElementReader, Master, ParseStep, Phase,
    SEGMENT, is_segment_metadata,
};
use crate::step::Required;
use crate::{Error, Result};
use std::sync::atomic::AtomicBool;
use triergon_core::{InputCursor, InputRequest, WorkBudget};

#[derive(Debug, Clone)]
pub(crate) struct ReplayLocation {
    checkpoint: ClusterCheckpoint,
    cluster: Master,
    position: u64,
    generation: u64,
    pub cluster_time: Option<u64>,
}
impl ReplayLocation {
    pub(crate) fn block_offset(&self) -> u64 {
        self.position
    }
    pub(crate) fn cluster_offset(&self) -> u64 {
        self.cluster.start
    }
    pub(crate) fn checkpoint(&self) -> &ClusterCheckpoint {
        &self.checkpoint
    }
}
impl ElementReader {
    #[inline(always)]
    pub(crate) fn healthy_for_job(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<()> {
        self.input.read_into(&mut [], work, cancel)?;
        if self.failed {
            return Err(Error::Invalid("reader requires explicit reset after error"));
        }
        Ok(())
    }
    /// Only the typed controller may borrow this while its envelope state is
    /// suspended. It must restore an envelope context before resuming step().
    pub(crate) fn range_input(&mut self) -> &mut InputCursor {
        &mut self.input
    }
    fn check_anchor(&self, anchor: &ClusterCheckpoint, offset: u64) -> Result<()> {
        if !anchor.pool.same_pool(self.input.budget()) {
            return Err(Error::ForeignPool);
        }
        if anchor.parser != self.parser || anchor.source != self.input.source() {
            return Err(Error::SourceMismatch);
        }
        if !self.input.capabilities().random_access {
            return Err(Error::Unsupported("random-access input"));
        }
        if offset < anchor.segment.start || anchor.segment.end.is_some_and(|end| offset >= end) {
            return Err(Error::Invalid("range outside Segment"));
        }
        Ok(())
    }
    fn install_cluster_range(&mut self, anchor: &ClusterCheckpoint) {
        self.phase = Phase::Between;
        self.segment = Some(anchor.segment);
        self.cluster = None;
        self.pending_header = None;
        self.seen_ebml = true;
        self.seen_segment = true;
        self.elements_left = self.limits.elements;
        self.scan_left = self.limits.scan_bytes;
        self.failed = false;
        self.expected_cluster = true;
    }
    pub(crate) fn begin_internal_cluster_range(
        &mut self,
        anchor: &ClusterCheckpoint,
        offset: u64,
    ) -> Result<InputRequest> {
        self.check_anchor(anchor, offset)?;
        let request = self.input.request_range(offset)?;
        self.install_cluster_range(anchor);
        Ok(request)
    }
    fn replay_block_context(&self, header: ElementHeader) -> Result<(Master, Master)> {
        if !matches!(header.id, 0xa0 | 0xa3) || header.generation != self.generation() {
            return Err(Error::Invalid("invalid replay block identity"));
        }
        let cluster = self.cluster.required("replay block outside Cluster")?;
        if header.start < cluster.start {
            return Err(Error::Invalid("replay block outside Cluster"));
        }
        self.check_parent(header, cluster.end)?;
        let segment = self.segment.required("missing replay Cluster context")?;
        Ok((cluster, segment))
    }
    fn replay_location_from_context(
        &self,
        header: ElementHeader,
        cluster_time: Option<u64>,
        cluster: Master,
        segment: Master,
    ) -> ReplayLocation {
        ReplayLocation {
            checkpoint: ClusterCheckpoint {
                pool: self.input.budget().clone(),
                parser: self.parser,
                source: self.input.source(),
                offset: cluster.start,
                segment,
            },
            cluster,
            position: header.start,
            generation: self.generation(),
            cluster_time,
        }
    }
    pub(crate) fn block_replay_location(
        &self,
        header: ElementHeader,
        cluster_time: Option<u64>,
    ) -> Result<ReplayLocation> {
        let (cluster, segment) = self.replay_block_context(header)?;
        Ok(self.replay_location_from_context(header, cluster_time, cluster, segment))
    }
    /// Validate before mutating the existing value. Earlier cloned locations
    /// remain independent; only this slot's scalar wire context changes.
    #[inline]
    pub(crate) fn refresh_block_replay_location(
        &self,
        slot: &mut Option<ReplayLocation>,
        header: ElementHeader,
        cluster_time: Option<u64>,
    ) -> Result<()> {
        let (cluster, segment) = self.replay_block_context(header)?;
        if let Some(location) = slot.as_mut().filter(|location| {
            location.checkpoint.pool.same_pool(self.input.budget())
                && location.checkpoint.parser == self.parser
                && location.checkpoint.source == self.input.source()
                && location.generation == self.generation()
        }) {
            location.checkpoint.offset = cluster.start;
            location.checkpoint.segment = segment;
            location.cluster = cluster;
            location.position = header.start;
            location.generation = self.generation();
            location.cluster_time = cluster_time;
        } else {
            *slot = Some(self.replay_location_from_context(header, cluster_time, cluster, segment));
        }
        Ok(())
    }
    pub(crate) fn restore_internal_block(
        &mut self,
        location: &ReplayLocation,
    ) -> Result<InputRequest> {
        self.check_anchor(&location.checkpoint, location.position)?;
        if location.generation != self.generation() {
            return Err(Error::StaleGeneration {
                expected: self.generation(),
                received: location.generation,
            });
        }
        let request = self.input.request_range(location.position)?;
        self.phase = Phase::Between;
        self.segment = Some(location.checkpoint.segment);
        self.cluster = Some(location.cluster);
        self.pending_header = None;
        self.seen_ebml = true;
        self.seen_segment = true;
        self.elements_left = self.limits.elements;
        self.scan_left = self.limits.scan_bytes;
        self.failed = false;
        self.expected_cluster = false;
        Ok(request)
    }
    /// Keep one scan limit for the whole seek operation. The existing normal
    /// packet reader's per-block refill cannot refill this remaining count.
    pub(crate) fn step_seek_operation(
        &mut self,
        remaining: &mut u64,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep> {
        self.scan_left = *remaining;
        let before = self.input.position();
        let result = self.step(work, cancel);
        let consumed = self
            .input
            .position()
            .checked_sub(before)
            .required("range changed inside envelope step")?;
        let Some(left) = remaining.checked_sub(consumed) else {
            self.failed = true;
            return Err(Error::Policy("operation read bytes"));
        };
        *remaining = left;
        self.scan_left = left;
        result
    }
}

/// A finite Cluster header found by [`ElementReader::owned_plain_prefix`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct PrefixBoundary {
    pub consumed: u64,
    pub data: u64,
    pub end: u64,
}
impl ElementReader {
    /// Checks a complete in-memory Cluster prefix with this envelope's own
    /// structure and limit rules, from the Cluster at `start` up to and
    /// including the header of the Cluster at `boundary`. Only finite Clusters
    /// whose children are SimpleBlocks, Timestamps and other non-master leaves
    /// qualify; segment-level Void/CRC-32 are skipped. Any BlockGroup, unknown
    /// size, level-one element, malformed/truncated header, limit or parent
    /// violation returns `None`, leaving the ordinary scan to report it.
    pub(crate) fn owned_plain_prefix(
        &self,
        root: &[u8],
        start: u64,
        boundary: u64,
        segment_end: Option<u64>,
        scan_left: u64,
        cancel: &AtomicBool,
    ) -> Option<PrefixBoundary> {
        let header = |at: u64| -> Option<(u64, Option<u64>, u64)> {
            let bytes = root.get(usize::try_from(at).ok()?..)?;
            let head = crate::ebml::header(bytes).ok()?;
            let data = at.checked_add(head.len as u64)?;
            Some((head.id, head.size, data))
        };
        let mut left = scan_left;
        let mut elements = 0_u64;
        let consume = |bytes: u64, left: &mut u64| -> Option<()> {
            *left = left.checked_sub(bytes)?;
            Some(())
        };
        let mut position = start;
        loop {
            let (id, size, data) = header(position)?;
            // The ordinary range reader requires the anchor to be a Cluster.
            if position == start && id != CLUSTER {
                return None;
            }
            elements += 1;
            if elements.is_multiple_of(1024) && cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            if elements >= self.limits.elements {
                return None;
            }
            consume(data - position, &mut left)?;
            let size = size?;
            let end = data.checked_add(size)?;
            if segment_end.is_some_and(|limit| data > limit || end > limit) {
                return None;
            }
            match id {
                CLUSTER if position == boundary => {
                    return Some(PrefixBoundary {
                        consumed: data - start,
                        data,
                        end,
                    });
                }
                CLUSTER if position < boundary => {
                    let mut child = data;
                    while child < end {
                        let (id, size, child_data) = header(child)?;
                        elements += 1;
                        if elements.is_multiple_of(1024)
                            && cancel.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            return None;
                        }
                        if elements >= self.limits.elements {
                            return None;
                        }
                        consume(child_data - child, &mut left)?;
                        let size = size?;
                        let child_end = child_data.checked_add(size)?;
                        if child_end > end || size > left {
                            return None;
                        }
                        match id {
                            0xa3 if size <= self.limits.block_bytes as u64 => (),
                            0xe7 if size <= 8 => (),
                            CLUSTER | SEGMENT | EBML | 0xa0 | 0xa3 | 0xe7 => return None,
                            id if is_segment_metadata(id) => return None,
                            _ => (),
                        }
                        consume(size, &mut left)?;
                        child = child_end;
                    }
                    position = end;
                }
                0xec | 0xbf if size <= left => {
                    consume(size, &mut left)?;
                    position = end;
                }
                _ => return None,
            }
        }
    }
    /// The existing seek byte policy, with the ordinary typed leaf admission
    /// boundary exposed to the state-history job. One parser and one cursor.
    /// With `admission`, that job's own admission rule is applied inside the
    /// envelope instead of pausing for it after each leaf header.
    pub(crate) fn step_typed_seek_operation(
        &mut self,
        remaining: &mut u64,
        admission: Option<super::EpochAdmission>,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<EnvelopeProgress> {
        self.scan_left = *remaining;
        let before = self.input.position();
        let result = self.step_typed_envelope_admitting(admission, work, cancel);
        let consumed = self
            .input
            .position()
            .checked_sub(before)
            .required("range changed inside envelope step")?;
        let Some(left) = remaining.checked_sub(consumed) else {
            self.failed = true;
            return Err(Error::Policy("operation read bytes"));
        };
        *remaining = left;
        self.scan_left = left;
        result
    }
}

#[cfg(test)]
mod tests;
