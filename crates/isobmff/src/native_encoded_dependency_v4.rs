//! The V4 SDTP view does not imply decoder recovery. This bounded source walk
//! admits only absent or explicitly unassociated roll/prol contexts. Existing
//! typed recovery parsers validate their own source grammar; one real mapping
//! row is inspected per work unit, with no sample array or inferred defaults.
use crate::{
    BoxChildren, BoxStep, Limits, RecoveryDescriptions, RecoveryLimits, RecoveryMappings,
    RecoverySyntax, SampleTable, fields,
};
use triergon_core::{Error, Result, WorkBudget};

pub(crate) struct EmptyRecoveryWalkV4 {
    children: Option<BoxChildren>,
    pending: Option<MappingRows>,
    seen: u8,
    samples: u64,
    limits: RecoveryLimits,
}
struct MappingRows {
    source: RecoveryMappings,
    row: u32,
    covered: u64,
}
impl EmptyRecoveryWalkV4 {
    pub(crate) fn new(table: &SampleTable, maximum: u64) -> Result<Self> {
        Ok(Self {
            children: Some(table.raw().children(0, Limits::default())?),
            pending: None,
            seen: 0,
            samples: table.sample_count(),
            limits: RecoveryLimits {
                max_descriptions: 64,
                max_runs: u32::try_from(maximum).unwrap_or(u32::MAX),
            },
        })
    }
    pub(crate) fn poll(&mut self, work: &mut WorkBudget) -> Result<bool> {
        loop {
            if let Some(pending) = &mut self.pending {
                if pending.row == pending.source.run_count() {
                    if pending.covered != self.samples {
                        return Err(Error::Unsupported(
                            "encoded V4 explicit group coverage profile",
                        ));
                    }
                    self.pending = None;
                    continue;
                }
                if !work.try_charge(1) {
                    return Ok(false);
                }
                let b = pending.source.raw().payload();
                // RecoveryMappings already validated this version and exact
                // row extent. These are the original row fields, not a cache.
                let records = if fields::full(b)?.0 == 0 {
                    12usize
                } else {
                    16usize
                };
                let at = usize::try_from(pending.row)
                    .map_err(|_| Error::Overflow)?
                    .checked_mul(8)
                    .and_then(|n| records.checked_add(n))
                    .ok_or(Error::Overflow)?;
                let count = fields::u32_at(b, at)?;
                let index = fields::u32_at(b, at + 4)?;
                if count == 0 {
                    return Err(Error::Invalid("encoded V4 empty group run"));
                }
                if index != 0 {
                    return Err(Error::Unsupported("encoded V4 actual recovery membership"));
                }
                pending.covered = pending
                    .covered
                    .checked_add(u64::from(count))
                    .ok_or(Error::Overflow)?;
                if pending.covered > self.samples {
                    return Err(Error::Invalid("encoded V4 group exceeds source samples"));
                }
                pending.row += 1;
                continue;
            }
            let Some(children) = &mut self.children else {
                return Ok(true);
            };
            match children.poll(work)? {
                BoxStep::Yield => return Ok(false),
                BoxStep::End => {
                    self.children = None;
                    return Ok(true);
                }
                BoxStep::Box(raw) => {
                    let description = match raw.header().kind {
                        x if x == *b"sgpd" => true,
                        x if x == *b"sbgp" => false,
                        _ => continue,
                    };
                    let kind = fields::fourcc(raw.payload(), 4)?;
                    let slot = match kind {
                        x if x == *b"roll" => 0,
                        x if x == *b"prol" => 1,
                        _ => {
                            return Err(Error::Unsupported(
                                "encoded V4 other sample-group semantics",
                            ));
                        }
                    } + if description { 0 } else { 2 };
                    let bit = 1u8 << slot;
                    if self.seen & bit != 0 {
                        return Err(Error::Invalid("encoded V4 duplicate recovery context"));
                    }
                    self.seen |= bit;
                    if description {
                        let parsed = RecoveryDescriptions::parse(
                            raw,
                            RecoverySyntax::Versions0And1,
                            self.limits,
                        )?;
                        if parsed.entry_count() != 0 {
                            return Err(Error::Unsupported(
                                "encoded V4 nonempty recovery descriptions",
                            ));
                        }
                    } else {
                        let parsed = RecoveryMappings::parse(raw, self.limits)?;
                        if parsed.grouping_parameter().is_some_and(|n| n != 0) {
                            return Err(Error::Unsupported(
                                "encoded V4 recovery grouping parameter",
                            ));
                        }
                        self.pending = Some(MappingRows {
                            source: parsed,
                            row: 0,
                            covered: 0,
                        });
                    }
                }
            }
        }
    }
}
