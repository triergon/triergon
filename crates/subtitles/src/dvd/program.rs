//! Cooperative interpretation of traditional SPU control programs.
use super::wire::Commands;
use super::{Command, DvdArea, DvdDate, DvdPacket, DvdRasterJob, DvdRasterLimits, FieldOffsets};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub struct ProgramLimits {
    pub max_packet_bytes: usize,
    pub max_controls: usize,
    pub max_commands: usize,
    pub max_commands_per_control: usize,
    pub max_work: usize,
}

impl Default for ProgramLimits {
    fn default() -> Self {
        Self {
            max_packet_bytes: usize::from(u16::MAX),
            max_controls: 1024,
            max_commands: 65536,
            max_commands_per_control: 8192,
            max_work: 1024 * 1024,
        }
    }
}

/// Last explicit assignments, including caller-provided initial facts.
/// Absence is not a receiver default or a claim about presentation lifetime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlState {
    pub palette_word: Option<u16>,
    pub alpha_word: Option<u16>,
    pub area: Option<DvdArea>,
    pub fields: Option<FieldOffsets>,
}

/// Strict source validation is the default. The explicit receiver profile
/// retains malformed source evidence while exposing the pinned d32 outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ControlPolicy {
    #[default]
    Strict,
    NativeD32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TruncatedControl {
    pub control_offset: u16,
    pub command_offset: u16,
    pub opcode: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpaqueCommandSpan {
    pub opcode: u8,
    pub operands_start: u16,
    pub operands_end: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlFacts {
    pub raw_start: u16,
    pub raw_end: u16,
    pub date: DvdDate,
    pub next_offset: u16,
    pub commands_start: u16,
    pub commands_end: u16,
    pub command_count: usize,
    pub starts: usize,
    pub forced_starts: usize,
    pub stops: usize,
    pub local_palette_word: Option<u16>,
    pub local_alpha_word: Option<u16>,
    pub local_area: Option<DvdArea>,
    pub local_fields: Option<FieldOffsets>,
    pub state_after: ControlState,
    pub opaque: Option<OpaqueCommandSpan>,
    /// The raw link remains in next_offset; no repaired source link is invented.
    pub receiver_stopped_at_link: bool,
}

#[derive(Debug)]
pub struct ControlProgram {
    raw: Bytes,
    first_control: usize,
    controls: BudgetVec<ControlFacts>,
    initial: ControlState,
    after: ControlState,
    last_start: Option<DvdDate>,
    last_stop: Option<DvdDate>,
    forced: bool,
    policy: ControlPolicy,
    truncated: Option<TruncatedControl>,
}

impl ControlProgram {
    pub(super) fn first_control_offset(&self) -> usize {
        self.first_control
    }

    pub(super) fn into_control_storage(mut self) -> BudgetVec<ControlFacts> {
        self.controls.clear();
        self.controls
    }

    pub fn raw(&self) -> &Bytes {
        &self.raw
    }
    pub fn controls(&self) -> &[ControlFacts] {
        self.controls.as_slice()
    }
    pub fn initial_state(&self) -> ControlState {
        self.initial
    }
    pub fn state_after(&self) -> ControlState {
        self.after
    }
    pub fn last_start(&self) -> Option<DvdDate> {
        self.last_start
    }
    pub fn last_stop(&self) -> Option<DvdDate> {
        self.last_stop
    }
    pub fn has_forced_start(&self) -> bool {
        self.forced
    }
    pub fn policy(&self) -> ControlPolicy {
        self.policy
    }
    pub fn truncated_control(&self) -> Option<TruncatedControl> {
        self.truncated
    }

    /// Read the validated presentation commands in wire order, including
    /// multiple commands at the same date. Opaque and truncated tails remain
    /// observable in the source program.
    pub fn display_controls(&self) -> Result<DisplayControls<'_>> {
        Ok(DisplayControls {
            packet: DvdPacket::parse(&self.raw)?,
            controls: self.controls(),
            policy: self.policy,
            index: 0,
            command: 0,
            offset: 0,
        })
    }

    pub fn raw_control(&self, index: usize) -> Result<&[u8]> {
        let c = self
            .controls
            .as_slice()
            .get(index)
            .ok_or(Error::Invalid("DVD control index"))?;
        self.raw
            .get(usize::from(c.raw_start)..usize::from(c.raw_end))
            .ok_or(Error::Invalid("DVD raw control span"))
    }

    /// Creates a raster only from a control's explicitly local area and fields.
    /// The complete source state remains available separately; this does not
    /// infer display scheduling or silently inherit absent local geometry.
    pub fn local_raster(
        &self,
        index: usize,
        budget: &Budget,
        limits: DvdRasterLimits,
    ) -> Result<Option<DvdRasterJob>> {
        let c = self
            .controls
            .as_slice()
            .get(index)
            .ok_or(Error::Invalid("DVD control index"))?;
        match (c.local_area, c.local_fields) {
            (Some(area), Some(fields)) => {
                DvdRasterJob::new(self.raw.clone(), area, fields, budget, limits).map(Some)
            }
            _ => Ok(None),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayAction {
    Show,
    ForcedShow,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayControl {
    pub index: usize,
    pub date: DvdDate,
    pub action: DisplayAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayControlStep {
    Command(DisplayControl),
    Yield,
    End,
}

pub struct DisplayControls<'a> {
    packet: DvdPacket<'a>,
    controls: &'a [ControlFacts],
    policy: ControlPolicy,
    index: usize,
    command: usize,
    offset: usize,
}

impl DisplayControls<'_> {
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DisplayControlStep> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let Some(fact) = self.controls.get(self.index) else {
                return Ok(DisplayControlStep::End);
            };
            if self.command == fact.command_count {
                self.index += 1;
                self.command = 0;
                self.offset = 0;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(DisplayControlStep::Yield);
            }
            let at = usize::from(fact.raw_start);
            let control = match self.policy {
                ControlPolicy::Strict => self.packet.control_at(at, fact.command_count)?,
                ControlPolicy::NativeD32 => {
                    self.packet.receiver_control_at(at, fact.command_count)?
                }
            };
            let (command, next, _) = control.command_at(self.offset, self.command)?;
            self.offset = next;
            self.command += 1;
            let action = match command {
                Command::Start => DisplayAction::Show,
                Command::ForcedStart => DisplayAction::ForcedShow,
                Command::Stop => DisplayAction::Clear,
                _ => continue,
            };
            return Ok(DisplayControlStep::Command(DisplayControl {
                index: self.index,
                date: fact.date,
                action,
            }));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStatus {
    Yield,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Packet,
    Header,
    Command,
    ReserveFact,
    AppendFact,
    Ready,
    Finished,
}

/// Strict validates links; NativeD32 records bounded receiver traversal and
/// termination while retaining the original links. Each command is charged once.
/// Every original byte remains, including END tails and unsupported operands.
/// Ordered commands can be reconstructed from raw spans without a command Vec.
#[derive(Debug)]
pub struct ProgramJob {
    raw: Option<Bytes>,
    records: Option<BudgetVec<ControlFacts>>,
    limits: ProgramLimits,
    initial: ControlState,
    state: ControlState,
    current: Option<ControlFacts>,
    first_control: usize,
    next: usize,
    command_at: usize,
    commands: usize,
    last_start: Option<DvdDate>,
    last_stop: Option<DvdDate>,
    forced: bool,
    work: usize,
    phase: Phase,
    failed: Option<Error>,
    policy: ControlPolicy,
    truncated: Option<TruncatedControl>,
}

impl ProgramJob {
    pub fn new(raw: Bytes, budget: &Budget, limits: ProgramLimits) -> Result<Self> {
        Self::with_state(raw, budget, limits, ControlState::default())
    }

    pub fn with_state(
        raw: Bytes,
        budget: &Budget,
        limits: ProgramLimits,
        initial: ControlState,
    ) -> Result<Self> {
        Self::with_policy(raw, budget, limits, initial, ControlPolicy::Strict)
    }

    pub fn with_policy(
        raw: Bytes,
        budget: &Budget,
        limits: ProgramLimits,
        initial: ControlState,
        policy: ControlPolicy,
    ) -> Result<Self> {
        Self::with_control_storage(raw, budget, limits, initial, policy, BudgetVec::new(budget))
    }

    pub(super) fn with_control_storage(
        raw: Bytes,
        budget: &Budget,
        limits: ProgramLimits,
        initial: ControlState,
        policy: ControlPolicy,
        mut records: BudgetVec<ControlFacts>,
    ) -> Result<Self> {
        if !raw.belongs_to(budget) || !records.budget().same_pool(budget) {
            return Err(Error::ForeignPool);
        }
        if raw.len() > limits.max_packet_bytes {
            return Err(Error::Limit {
                requested: raw.len(),
                maximum: limits.max_packet_bytes,
            });
        }
        records.clear();
        Ok(Self {
            raw: Some(raw),
            records: Some(records),
            limits,
            initial,
            state: initial,
            current: None,
            first_control: 0,
            next: 0,
            command_at: 0,
            commands: 0,
            last_start: None,
            last_stop: None,
            forced: false,
            work: 0,
            phase: Phase::Packet,
            failed: None,
            policy,
            truncated: None,
        })
    }

    pub fn work_spent(&self) -> usize {
        self.work
    }
    pub fn commands_processed(&self) -> usize {
        self.commands
    }
    pub fn controls_processed(&self) -> usize {
        self.records.as_ref().map_or(0, |r| r.len())
    }

    fn poison(&mut self, error: Error) -> Error {
        self.raw = None;
        self.records = None;
        self.current = None;
        self.failed = Some(error);
        error
    }

    fn pay(spent: &mut usize, maximum: usize, work: &mut WorkBudget) -> Result<bool> {
        if work.remaining() == 0 {
            return Ok(false);
        }
        let next = spent.checked_add(1).ok_or(Error::Overflow)?;
        if next > maximum {
            return Err(Error::Limit {
                requested: next,
                maximum,
            });
        }
        work.take(1);
        *spent = next;
        Ok(true)
    }

    fn packet(&self) -> Result<DvdPacket<'_>> {
        DvdPacket::parse(
            self.raw
                .as_deref()
                .ok_or(Error::Invalid("DVD program source absent"))?,
        )
    }

    fn control(&self, at: usize) -> Result<super::Control<'_>> {
        let raw = self
            .raw
            .as_deref()
            .ok_or(Error::Invalid("DVD program source absent"))?;
        // Packet validation already established the control offset on this
        // immutable input before entering the first header phase.
        let packet = DvdPacket::with_control_offset(raw, self.first_control);
        match self.policy {
            ControlPolicy::Strict => packet.control_at(at, self.limits.max_commands_per_control),
            ControlPolicy::NativeD32 => {
                packet.receiver_control_at(at, self.limits.max_commands_per_control)
            }
        }
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ProgramStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase == Phase::Ready {
            return Ok(ProgramStatus::Ready);
        }
        if self.phase == Phase::Finished {
            return Err(self.poison(Error::Invalid("DVD program already finished")));
        }
        match self.inner(work, cancel) {
            Ok(status) => Ok(status),
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
        }
    }

    fn interpret_block(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        let fact = self
            .current
            .as_mut()
            .ok_or(Error::Invalid("DVD current control absent"))?;
        let raw = self
            .raw
            .as_ref()
            .ok_or(Error::Invalid("DVD program source absent"))?;
        let start = usize::from(fact.commands_start);
        let input = raw
            .get(start..usize::from(fact.raw_end))
            .ok_or(Error::Invalid("DVD command bounds absent"))?;
        let mut commands = Commands::resume(
            input,
            self.command_at,
            fact.command_count,
            self.limits.max_commands_per_control,
        );
        let available = work.remaining();
        let funded = available.min((self.limits.max_work - self.work) as u64);
        let mut charged = 0;
        let mut valid_end = self.command_at;
        let outcome = (|| loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if charged == funded {
                if charged < available {
                    return Err(Error::Limit {
                        requested: self.limits.max_work.checked_add(1).ok_or(Error::Overflow)?,
                        maximum: self.limits.max_work,
                    });
                }
                self.command_at = commands.position();
                break Ok(false);
            }
            charged += 1;
            if self.commands == self.limits.max_commands {
                return Err(Error::Limit {
                    requested: self.commands.checked_add(1).ok_or(Error::Overflow)?,
                    maximum: self.limits.max_commands,
                });
            }
            let at = commands.position();
            let command = match commands
                .next()
                .ok_or(Error::Invalid("DVD command cursor ended"))?
            {
                Ok(command) => command,
                Err(Error::Truncated) if self.policy == ControlPolicy::NativeD32 => {
                    let command_offset = start.checked_add(at).ok_or(Error::Overflow)?;
                    self.truncated = Some(TruncatedControl {
                        control_offset: fact.raw_start,
                        command_offset: u16::try_from(command_offset)
                            .map_err(|_| Error::Overflow)?,
                        opcode: raw.get(command_offset).copied(),
                    });
                    break Ok(true);
                }
                Err(error) => return Err(error),
            };
            fact.command_count = fact.command_count.checked_add(1).ok_or(Error::Overflow)?;
            valid_end = commands.position();
            match command {
                Command::Start => {
                    fact.starts += 1;
                    self.last_start = Some(fact.date);
                }
                Command::ForcedStart => {
                    fact.forced_starts += 1;
                    self.forced = true;
                }
                Command::Stop => {
                    fact.stops += 1;
                    self.last_stop = Some(fact.date);
                }
                Command::Palette(word) => {
                    fact.local_palette_word = Some(word);
                    self.state.palette_word = Some(word);
                }
                Command::Alpha(word) => {
                    fact.local_alpha_word = Some(word);
                    self.state.alpha_word = Some(word);
                }
                Command::Area(area) => {
                    fact.local_area = Some(area);
                    self.state.area = Some(area);
                }
                Command::Fields(fields) => {
                    fact.local_fields = Some(fields);
                    self.state.fields = Some(fields);
                }
                Command::Opaque { opcode, .. } => {
                    fact.opaque = Some(OpaqueCommandSpan {
                        opcode,
                        operands_start: (start + valid_end) as u16,
                        operands_end: fact.raw_end,
                    });
                }
                Command::End => (),
            }
            self.commands += 1;
            if commands.is_done() {
                break Ok(true);
            }
        })();
        work.take(charged);
        self.work += charged as usize;
        // The command cursor is bounded by this validated 16-bit SPU span.
        fact.commands_end = (start + valid_end) as u16;
        outcome
    }

    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ProgramStatus> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(ProgramStatus::Yield);
            }
            match self.phase {
                Phase::Packet => {
                    if !Self::pay(&mut self.work, self.limits.max_work, work)? {
                        return Ok(ProgramStatus::Yield);
                    }
                    self.first_control = self.packet()?.control_offset();
                    self.next = self.first_control;
                    self.phase = Phase::Header;
                }
                Phase::Header => {
                    if !Self::pay(&mut self.work, self.limits.max_work, work)? {
                        return Ok(ProgramStatus::Yield);
                    }
                    let count = self
                        .records
                        .as_ref()
                        .ok_or(Error::Invalid("DVD program records absent"))?
                        .len();
                    if count == self.limits.max_controls {
                        return Err(Error::Limit {
                            requested: count.checked_add(1).ok_or(Error::Overflow)?,
                            maximum: self.limits.max_controls,
                        });
                    }
                    let control = self.control(self.next)?;
                    let start = control.offset.checked_add(4).ok_or(Error::Overflow)?;
                    let end = control
                        .offset
                        .checked_add(control.raw.len())
                        .ok_or(Error::Overflow)?;
                    self.current = Some(ControlFacts {
                        raw_start: u16::try_from(control.offset).map_err(|_| Error::Overflow)?,
                        raw_end: u16::try_from(end).map_err(|_| Error::Overflow)?,
                        date: control.date,
                        next_offset: control.next_offset,
                        commands_start: u16::try_from(start).map_err(|_| Error::Overflow)?,
                        commands_end: u16::try_from(start).map_err(|_| Error::Overflow)?,
                        command_count: 0,
                        starts: 0,
                        forced_starts: 0,
                        stops: 0,
                        local_palette_word: None,
                        local_alpha_word: None,
                        local_area: None,
                        local_fields: None,
                        state_after: self.state,
                        opaque: None,
                        receiver_stopped_at_link: false,
                    });
                    self.command_at = 0;
                    self.phase = Phase::Command;
                }
                Phase::Command => {
                    if !self.interpret_block(work, cancel)? {
                        return Ok(ProgramStatus::Yield);
                    }
                    self.phase = Phase::ReserveFact;
                }
                Phase::ReserveFact => {
                    if !Self::pay(&mut self.work, self.limits.max_work, work)? {
                        return Ok(ProgramStatus::Yield);
                    }
                    // The setup charge remains paid through allocation pressure.
                    self.phase = Phase::AppendFact;
                }
                Phase::AppendFact => {
                    let mut fact = self
                        .current
                        .ok_or(Error::Invalid("DVD current control absent"))?;
                    fact.state_after = self.state;
                    let receiver_stop =
                        self.policy == ControlPolicy::NativeD32
                            && (fact.next_offset < fact.raw_start
                                || usize::from(fact.next_offset).checked_add(5).is_none_or(
                                    |end| end > self.raw.as_ref().map_or(0, |raw| raw.len()),
                                ));
                    fact.receiver_stopped_at_link = receiver_stop;
                    self.records
                        .as_mut()
                        .ok_or(Error::Invalid("DVD program records absent"))?
                        .try_push(fact)
                        .map_err(|e| e.error)?;
                    self.current = None;
                    if self.truncated.is_some()
                        || receiver_stop
                        || fact.next_offset == fact.raw_start
                    {
                        self.phase = Phase::Ready;
                    } else {
                        self.next = usize::from(fact.next_offset);
                        self.phase = Phase::Header;
                    }
                }
                Phase::Ready => return Ok(ProgramStatus::Ready),
                Phase::Finished => return Err(Error::Invalid("DVD program already finished")),
            }
        }
    }

    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<ControlProgram> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if self.phase != Phase::Ready {
            return Err(self.poison(Error::Invalid("DVD program not ready")));
        }
        let raw = self
            .raw
            .take()
            .ok_or(Error::Invalid("DVD program source absent"))?;
        let controls = self
            .records
            .take()
            .ok_or(Error::Invalid("DVD program records absent"))?;
        self.phase = Phase::Finished;
        Ok(ControlProgram {
            raw,
            first_control: self.first_control,
            controls,
            initial: self.initial,
            after: self.state,
            last_start: self.last_start,
            last_stop: self.last_stop,
            forced: self.forced,
            policy: self.policy,
            truncated: self.truncated,
        })
    }
}
