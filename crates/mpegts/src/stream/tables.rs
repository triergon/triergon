//! Program tables and the routes they bind.
use super::event::{FaultKind, TraceEvent, TraceFault};
use super::{PesRoute, PsiRoute, Route, TransportStream, retryable};
use crate::SourceEpochs;
use crate::pes::PesAssembler;
use crate::psi::{PatReader, PatTable, PmtReader, ProgramMap, TableStep};
use crate::transport::Continuity;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "one bounded in-progress source table parser; no extra boxed allocator outside Budget"
)]
pub(super) enum Job {
    Pat(PatReader),
    Pmt(PmtReader),
}
#[derive(Debug)]
pub(super) struct MapBuild {
    map: ProgramMap,
    routes: BudgetVec<PesRoute>,
    position: usize,
    epoch: u64,
}

impl TransportStream {
    pub(super) fn find_route(&self, pid: u16) -> Route {
        if pid == 0 {
            Route::Pat
        } else if self.pmt.as_ref().is_some_and(|pmt| pmt.pid == pid) {
            Route::Pmt
        } else if let Some(index) = self
            .routes
            .as_slice()
            .iter()
            .position(|route| route.pid == pid)
        {
            Route::Pes(index)
        } else if self.pcr_pid == Some(pid) {
            Route::Clock
        } else {
            Route::Ignore
        }
    }
    pub(super) fn route_for_carrier(&self, pid: u16) -> Route {
        // Elementary PIDs are unique. PAT/PMT changes clear or replace the
        // table before another carrier is read; recheck the indexed PID.
        if let Route::Pes(index) = self.route
            && self
                .routes
                .as_slice()
                .get(index)
                .is_some_and(|route| route.pid == pid)
        {
            return self.route;
        }
        self.find_route(pid)
    }
    pub(super) fn discard_route(&mut self, route: Route) {
        match route {
            Route::Pat => self.pat.assembler.discard(),
            Route::Pmt => {
                if let Some(pmt) = self.pmt.as_mut() {
                    pmt.assembler.discard();
                }
            }
            Route::Pes(index) => {
                if let Some(PesRoute {
                    assembler: Some(assembler),
                    epochs,
                    ..
                }) = self.routes.as_mut_slice().get_mut(index)
                {
                    assembler.discard(*epochs);
                }
            }
            Route::Clock | Route::Ignore => {}
        }
    }
    fn apply_pat(&mut self, table: &PatTable) -> Result<()> {
        let programs = table.programs.as_slice();
        let entry = if self.discover_program {
            let mut media = programs.iter().filter(|entry| entry.program_number != 0);
            let entry = media
                .next()
                .ok_or(Error::Invalid("segment PAT has no media program"))?;
            if media.next().is_some() {
                return Err(Error::Unsupported(
                    "segment PAT has multiple media programs",
                ));
            }
            if self.program != 0 && self.program != entry.program_number {
                return Err(Error::Unsupported("segment PAT changed media program"));
            }
            Some(entry)
        } else {
            programs
                .iter()
                .find(|entry| entry.program_number == self.program)
        };
        let pid = entry.map(|entry| entry.pid);
        if self.pmt.as_ref().map(|pmt| pmt.pid) != pid || self.tsid != Some(table.header.extension)
        {
            let epoch = self.config_epoch.checked_add(1).ok_or(Error::Overflow)?;
            let pmt = pid
                .map(|pid| PsiRoute::new(pid, &self.budget, self.limits))
                .transpose()?;
            self.pmt = pmt;
            self.routes = BudgetVec::new(&self.budget);
            self.pmt_source = None;
            self.pmt_version = None;
            self.pcr_pid = None;
            self.clock_continuity = None;
            self.clock_pending = false;
            self.clock_signal = false;
            self.clock_samples = None;
            self.config_epoch = epoch;
        }
        if let Some(entry) = entry {
            self.program = entry.program_number;
        }
        self.tsid = Some(table.header.extension);
        Ok(())
    }
    fn poll_map(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<TraceEvent>> {
        let Some(build) = self.map_build.as_mut() else {
            return Ok(None);
        };
        while build.position < build.map.streams.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
            let stream = &build.map.streams.as_slice()[build.position];
            let reuse = self.routes.as_slice().iter().position(|old| {
                old.pid == stream.pid
                    && old.stream_type == stream.stream_type
                    && old.descriptors.as_ref() == stream.descriptor_bytes.as_ref()
            });
            let epochs = SourceEpochs {
                clock: self.clock_epoch,
                configuration: build.epoch,
            };
            let assembler = if reuse.is_some() {
                None
            } else {
                let mut assembler = PesAssembler::new(
                    stream.pid,
                    is_video_stream_type(stream.stream_type),
                    self.budget.clone(),
                    self.limits,
                    epochs,
                )?;
                if self.unbounded_adts && stream.stream_type == 0x0f {
                    assembler.allow_unbounded_adts();
                }
                Some(assembler)
            };
            build
                .routes
                .try_push(PesRoute {
                    pid: stream.pid,
                    stream_type: stream.stream_type,
                    descriptors: stream.descriptor_bytes.clone(),
                    epochs,
                    continuity: Some(Continuity::new(stream.pid)?),
                    assembler,
                    reuse,
                })
                .map_err(|error| error.error)?;
            build.position += 1;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        // Atomic commit: all allocations and validation precede these bounded moves.
        let mut build = self
            .map_build
            .take()
            .ok_or(Error::Invalid("PMT transaction missing"))?;
        for route in build.routes.as_mut_slice() {
            if let Some(index) = route.reuse.take() {
                let previous = &mut self.routes.as_mut_slice()[index];
                route.assembler = previous.assembler.take();
                route.continuity = previous.continuity.take();
                route.epochs = previous.epochs;
            }
        }
        let pcr = if build.map.pcr_pid == 0x1fff {
            None
        } else {
            Some(build.map.pcr_pid)
        };
        let clock = match pcr {
            Some(pid) => Some(Continuity::new(pid)?),
            None => None,
        };
        let binding_changed = self.pcr_pid != pcr;
        let pending_rebind = if binding_changed && self.clock_pending {
            Some(TraceFault {
                kind: FaultKind::Program,
                pid: self.pcr_pid,
                location: self.location(),
                error: Error::Invalid("pending clock signal changed PCR binding"),
                rejected: None,
            })
        } else {
            None
        };
        self.routes = build.routes;
        self.pmt_source = Some(build.map.source.clone());
        self.pmt_version = Some(build.map.header.version);
        self.pcr_pid = pcr;
        self.clock_continuity = clock;
        self.config_epoch = build.epoch;
        if binding_changed {
            self.clock_pending = false;
            self.clock_signal = false;
        }
        if let Some(fault) = pending_rebind {
            self.clock_quarantined = true;
            // poll_jobs emits this immediately after the committed raw PMT event.
            self.fault = Some(fault);
        }
        Ok(Some(TraceEvent::ProgramMap {
            map: build.map,
            active: true,
            configuration_epoch: build.epoch,
        }))
    }
    pub(super) fn poll_jobs(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<TraceEvent>> {
        if let Some(fault) = self.fault.take() {
            return Ok(Some(TraceEvent::Diagnostic(fault)));
        }
        if self.map_build.is_some() {
            return self.poll_map(work, cancel);
        }
        if let Some(section) = self.section.as_ref() {
            if section.crc_remainder() == Some(0)
                && self
                    .accepted_table_source()
                    .is_some_and(|current| current.as_ref() == section.bytes().as_ref())
            {
                // The active table already passed validation in this route.
                // Publish the repeat's section with its own source ranges.
                let section = self
                    .section
                    .take()
                    .ok_or(Error::Invalid("PSI source missing"))?;
                return Ok(Some(TraceEvent::Section(section)));
            }
            let job = match self.route {
                Route::Pat => PatReader::new(section, &self.budget, self.limits).map(Job::Pat),
                Route::Pmt => PmtReader::new_with_namespace(
                    section,
                    &self.budget,
                    self.limits,
                    self.descriptor_namespace,
                )
                .map(Job::Pmt),
                _ => return Err(Error::Invalid("PSI source without PSI route")),
            };
            match job {
                Ok(job) => self.job = Some(job),
                Err(error) if retryable(error) => return Err(error),
                Err(error) => {
                    self.fault = Some(TraceFault {
                        kind: FaultKind::Psi,
                        pid: Some(section.pid()),
                        location: self.location(),
                        error,
                        rejected: None,
                    });
                }
            }
            let section = self
                .section
                .take()
                .ok_or(Error::Invalid("PSI source missing"))?;
            return Ok(Some(TraceEvent::Section(section)));
        }
        if let Some(job) = self.job.as_mut() {
            match job {
                Job::Pat(reader) => match reader.poll(work, cancel) {
                    Ok(TableStep::Output(section)) => {
                        self.job = None;
                        self.collector
                            .submit(section)
                            .map_err(|error| error.error)?;
                    }
                    Ok(TableStep::Yield) => return Ok(None),
                    Ok(TableStep::Done) => self.job = None,
                    Err(error) if retryable(error) => return Err(error),
                    Err(error) => {
                        self.job = None;
                        return Ok(Some(self.diagnostic(FaultKind::Psi, Some(0), error)));
                    }
                },
                Job::Pmt(reader) => match reader.poll(work, cancel) {
                    Ok(TableStep::Output(map)) => {
                        self.job = None;
                        if map.header.extension != self.program {
                            return Ok(Some(self.diagnostic(
                                FaultKind::Program,
                                self.pmt.as_ref().map(|pmt| pmt.pid),
                                Error::Invalid("PMT program differs from active PAT"),
                            )));
                        }
                        if !map.header.current_next {
                            return Ok(Some(TraceEvent::ProgramMap {
                                map,
                                active: false,
                                configuration_epoch: self.config_epoch,
                            }));
                        }
                        if self
                            .pmt_source
                            .as_ref()
                            .is_some_and(|old| old.as_ref() == map.source.as_ref())
                        {
                            return Ok(None);
                        }
                        if self.pmt_version == Some(map.header.version) {
                            return Ok(Some(self.diagnostic(
                                FaultKind::Program,
                                self.pmt.as_ref().map(|pmt| pmt.pid),
                                Error::Invalid("PMT conflicting repeat"),
                            )));
                        }
                        let epoch = self.config_epoch.checked_add(1).ok_or(Error::Overflow)?;
                        self.map_build = Some(MapBuild {
                            map,
                            routes: BudgetVec::new(&self.budget),
                            position: 0,
                            epoch,
                        });
                        return self.poll_map(work, cancel);
                    }
                    Ok(TableStep::Yield) => return Ok(None),
                    Ok(TableStep::Done) => self.job = None,
                    Err(error) if retryable(error) => return Err(error),
                    Err(error) => {
                        self.job = None;
                        return Ok(Some(self.diagnostic(
                            FaultKind::Psi,
                            self.pmt.as_ref().map(|pmt| pmt.pid),
                            error,
                        )));
                    }
                },
            }
        }
        match self.collector.poll(work, cancel) {
            Ok(TableStep::Output(table)) => {
                self.apply_pat(&table)?;
                Ok(Some(TraceEvent::Pat(table)))
            }
            Ok(TableStep::Yield | TableStep::Done) => Ok(None),
            Err(error) if retryable(error) => Err(error),
            Err(error) => {
                self.collector.discard();
                Ok(Some(self.diagnostic(FaultKind::Program, Some(0), error)))
            }
        }
    }
    pub(super) fn accepted_table_source(&self) -> Option<&Bytes> {
        match self.route {
            Route::Pat => self.collector.active_single_section(),
            Route::Pmt => self.pmt_source.as_ref(),
            _ => None,
        }
    }
}

/// Video stream types, whose PES packets may leave `PES_packet_length` 0
/// (ISO/IEC 13818-1 2.4.3.7): MPEG-1 and MPEG-2 video, H.264 and HEVC.
fn is_video_stream_type(stream_type: u8) -> bool {
    matches!(stream_type, 0x01 | 0x02 | 0x1b | 0x24)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParseLimits;
    use crate::psi::DescriptorNamespace;
    use triergon_core::Budget;

    fn stream() -> TransportStream {
        let pool = Budget::new(64 * 1024).unwrap();
        let limits = ParseLimits::default();
        let mut stream = TransportStream::new_single_program_progressive_feed(
            pool.clone(),
            limits,
            DescriptorNamespace::Unspecified,
        )
        .unwrap();
        stream.pmt = Some(PsiRoute::new(100, &pool, limits).unwrap());
        stream.pcr_pid = Some(300);
        for pid in [256, 257] {
            stream
                .routes
                .try_push(PesRoute {
                    pid,
                    stream_type: 0x1b,
                    descriptors: Bytes::copy_from_slice(&pool, &[]).unwrap(),
                    epochs: stream.source_epochs(),
                    continuity: Some(Continuity::new(pid).unwrap()),
                    assembler: None,
                    reuse: None,
                })
                .unwrap();
        }
        stream
    }

    #[test]
    fn selected_feed_route_preserves_control_and_interleaved_pid_dispatch() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TransportStream>();
        let mut stream = stream();
        let expected = [
            (0, Route::Pat),
            (100, Route::Pmt),
            (256, Route::Pes(0)),
            (257, Route::Pes(1)),
            (300, Route::Clock),
            (0x1fff, Route::Ignore),
        ];
        for previous in [Route::Ignore, Route::Pmt, Route::Pes(0), Route::Pes(1)] {
            stream.route = previous;
            for (pid, expected) in expected {
                let actual = stream.route_for_carrier(pid);
                assert_eq!(actual, expected);
                stream.route = actual;
                assert_eq!(stream.route_for_carrier(pid), expected);
            }
        }
    }

    #[test]
    fn selected_feed_route_rechecks_replaced_and_cleared_tables() {
        let mut stream = stream();
        stream.route = Route::Pes(1);
        stream.routes.as_mut_slice().swap(0, 1);
        assert_eq!(stream.route_for_carrier(257), Route::Pes(0));
        assert_eq!(stream.route_for_carrier(256), Route::Pes(1));
        stream.route = Route::Pes(1);
        stream.routes.clear();
        assert_eq!(stream.route_for_carrier(256), Route::Ignore);
        assert_eq!(stream.route_for_carrier(100), Route::Pmt);
        assert_eq!(stream.route_for_carrier(300), Route::Clock);
    }
}
