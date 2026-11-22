use std::{ffi::OsString, io::Write, ops::Range};

use triergon_core::{Budget, Bytes, Error, Rounding, TimeBase, WorkBudget};
use triergon_isobmff::{
    Limits, Next,
    boxes::BoxView,
    index::{MovieFragmentRandomAccessTail, SegmentIndex, TrackFragmentRandomAccess},
};

use super::protocol::{Operation, Work};

#[derive(Clone, Copy)]
enum Kind {
    Sidx,
    Tfra,
    Mfro,
}

pub(super) fn operation<'a>(
    _: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [kind, offset, length, quantum] = arguments else {
        return Err("index needs a box kind, file offset, byte length and work quantum".into());
    };
    let kind = match kind.to_str() {
        Some("sidx") => Kind::Sidx,
        Some("tfra") => Kind::Tfra,
        Some("mfro") => Kind::Mfro,
        _ => return Err("unknown index box kind".into()),
    };
    let number = |arg: &OsString| arg.to_str().and_then(|s| s.parse::<usize>().ok());
    let offset = number(offset).ok_or("invalid index offset")?;
    let length = number(length).ok_or("invalid index length")?;
    let quantum = number(quantum)
        .filter(|q| *q > 0)
        .ok_or("invalid work quantum")? as u64;
    let end = offset.checked_add(length).ok_or("index extent overflow")?;
    Ok(Operation {
        name: "isobmff-segment-indexes",
        run: Box::new(move |work, trace| {
            let mut output = Output { work, trace };
            let pool = Budget::new(16 << 20).map_err(|e| e.to_string())?;
            let result = parsed_box(input, offset..end, &pool).and_then(|raw| match kind {
                Kind::Sidx => sidx(raw, quantum, &mut output),
                Kind::Tfra => tfra(raw, quantum, &mut output),
                Kind::Mfro => mfro(raw, input, &pool, &mut output),
            });
            if let Err(Error::Transport) = result {
                return Err("write index trace".into());
            }
            output
                .row(if result.is_ok() { 7 } else { 8 }, &[])
                .map_err(|e| e.to_string())
        }),
    })
}

pub(super) fn parsed_box(
    input: &[u8],
    range: Range<usize>,
    pool: &Budget,
) -> triergon_core::Result<BoxView> {
    let bytes = input.get(range.clone()).ok_or(Error::Truncated)?;
    BoxView::new(Bytes::copy_from_slice(pool, bytes)?, range.start as u64)
}

struct Output<'a, 'b> {
    work: &'a mut Work,
    trace: Option<&'b mut dyn Write>,
}

impl Output<'_, '_> {
    fn row(&mut self, kind: u64, fields: &[u64]) -> triergon_core::Result<()> {
        self.work.record(kind, fields, &[]);
        if let Some(out) = self.trace.as_deref_mut() {
            write!(out, "{kind}").map_err(|_| Error::Transport)?;
            for field in fields {
                write!(out, " {field}").map_err(|_| Error::Transport)?;
            }
            writeln!(out).map_err(|_| Error::Transport)?;
        }
        Ok(())
    }
}

fn sidx(raw: BoxView, quantum: u64, output: &mut Output<'_, '_>) -> triergon_core::Result<()> {
    let index = SegmentIndex::parse(raw, Limits::default())?;
    output.row(
        2,
        &[
            index.version().into(),
            index.reference_id().into(),
            index.timescale().into(),
            index.earliest_presentation_ticks(),
            index.first_offset(),
            index.reference_count().into(),
        ],
    )?;
    let base = TimeBase::new(1, u64::from(index.timescale()))?;
    let mut cursor = index.cursor();
    let mut work = WorkBudget::new(quantum);
    loop {
        match cursor.poll(&mut work)? {
            Next::Item(row) => {
                let tick = row
                    .earliest_presentation
                    .to_ticks(base, Rounding::TowardZero)?;
                let duration = row
                    .presentation_duration
                    .to_ticks(base, Rounding::TowardZero)?;
                output.row(
                    3,
                    &[
                        row.ordinal.into(),
                        row.references_index.into(),
                        row.range.start,
                        row.range.end,
                        (tick >> 64) as u64,
                        tick as u64,
                        duration as u64,
                        row.starts_with_sap.into(),
                        row.sap_type.into(),
                        row.sap_delta_ticks.into(),
                        row.raw_size_word.into(),
                        row.raw_sap_word.into(),
                    ],
                )?;
            }
            Next::Yield => work = WorkBudget::new(quantum),
            Next::End => return Ok(()),
        }
    }
}

fn tfra(raw: BoxView, quantum: u64, output: &mut Output<'_, '_>) -> triergon_core::Result<()> {
    let index = TrackFragmentRandomAccess::parse(raw, Limits::default())?;
    let [traf, trun, sample] = index.number_widths();
    output.row(
        2,
        &[
            index.version().into(),
            index.track_id().into(),
            index.entry_count().into(),
            traf.into(),
            trun.into(),
            sample.into(),
        ],
    )?;
    let mut cursor = index.cursor();
    let mut work = WorkBudget::new(quantum);
    loop {
        match cursor.poll(&mut work)? {
            Next::Item(row) => output.row(
                3,
                &[
                    row.ordinal.into(),
                    row.time_ticks,
                    row.moof_offset,
                    row.traf_number.into(),
                    row.trun_number.into(),
                    row.sample_number.into(),
                ],
            )?,
            Next::Yield => work = WorkBudget::new(quantum),
            Next::End => return Ok(()),
        }
    }
}

fn mfro(
    raw: BoxView,
    input: &[u8],
    pool: &Budget,
    output: &mut Output<'_, '_>,
) -> triergon_core::Result<()> {
    let tail = MovieFragmentRandomAccessTail::parse(raw, input.len() as u64)?;
    let range = tail.parent_range();
    output.row(
        2,
        &[
            tail.raw().payload()[0].into(),
            range.end - range.start,
            range.start,
            range.end,
        ],
    )?;
    let start = usize::try_from(range.start).map_err(|_| Error::Overflow)?;
    tail.require_mfra(&parsed_box(input, start..input.len(), pool)?)
}
