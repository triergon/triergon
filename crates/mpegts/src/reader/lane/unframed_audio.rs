//! Bounded admission for a private-audio PES with no syncframe start.
use super::Track;
use crate::Pes;
use crate::compat::pes::{PrivateAudioCarriage, PrivateAudioDescriptors, UnframedAudioPolicy};
use crate::psi::DescriptorNamespace;
use crate::reader::ReaderLimits;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

pub(super) const NEED_PES: Error = Error::Unsupported("unframed audio requires one finite PES");
const SYNC: Error = Error::Unsupported("unframed audio contains a syncframe start");

pub(super) struct Scan {
    pub(super) carriage: PrivateAudioCarriage,
    span: usize,
    offset: usize,
    sync_prefix: bool,
}

impl Scan {
    pub(super) fn for_track(track: &Track, limits: ReaderLimits) -> Result<Option<Self>> {
        let stream = track.stream();
        if limits.unframed_audio_policy == UnframedAudioPolicy::Strict
            || stream.descriptor_namespace() != DescriptorNamespace::Unspecified
            || !matches!(stream.stream_type, 0x06 | 0x81 | 0x87)
        {
            return Ok(None);
        }
        let descriptors = PrivateAudioDescriptors {
            program: track.program_map().program_descriptors.as_ref(),
            elementary: stream.descriptor_bytes.as_ref(),
        };
        match PrivateAudioCarriage::from_observed_pmt(
            stream.stream_type,
            descriptors,
            limits.transport,
        ) {
            Ok(carriage) => Ok(Some(Self {
                carriage,
                span: 0,
                offset: 0,
                sync_prefix: false,
            })),
            Err(Error::Unsupported(_) | Error::Invalid(_) | Error::Truncated) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn check_pes(pes: &Pes, first: bool, maximum: usize) -> Result<()> {
        if !first
            || pes.header.stream_id != 0xbd
            || pes.header.declared_packet_length == 0
            || pes.payload_len == 0
        {
            return Err(NEED_PES);
        }
        if pes.payload_len > maximum {
            return Err(crate::limit(pes.payload_len, maximum));
        }
        Ok(())
    }

    pub(super) fn step(
        &mut self,
        pes: &Pes,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while let Some(span) = pes.spans().get(self.span) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let bytes = span.as_ref();
            let count = work.take((bytes.len() - self.offset).min(4096) as u64) as usize;
            if count == 0 {
                return Ok(false);
            }
            let end = self.offset + count;
            for &byte in &bytes[self.offset..end] {
                if self.sync_prefix && byte == 0x77 {
                    return Err(SYNC);
                }
                self.sync_prefix = byte == 0x0b;
            }
            self.offset = end;
            if end == bytes.len() {
                self.span += 1;
                self.offset = 0;
            }
        }
        if self.sync_prefix {
            return Err(SYNC);
        }
        Ok(true)
    }
}
