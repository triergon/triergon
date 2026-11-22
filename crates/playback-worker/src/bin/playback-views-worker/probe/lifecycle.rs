//! Source ownership and sequential handoff around a playback probe.

use std::cell::RefCell;
use std::sync::atomic::AtomicBool;

use triergon_core::{Error, InputRequest, SourceFailure};
use triergon_playback::SourceReply;

use super::{
    Budget, ByteSource, Bytes, Format, ID, Invocation, MAX_INPUT, Mode, Probe, ProbeProgress,
    Result, Source, SourceCapabilities, Totals, WORK, WorkBudget, fold, fold_bytes, measure,
    read_replay,
};

const WINDOW: usize = 7;

struct Sequential<'a> {
    source: ByteSource,
    next: u64,
    pending: bool,
    fail: bool,
    calls: &'a RefCell<Vec<&'static str>>,
}

impl Source for Sequential<'_> {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if request.offset != self.next {
            return Err(Error::Invalid(
                "probe rewound or skipped its sequential source",
            ));
        }
        if self.fail {
            return Ok(SourceReply::Failed {
                source: request.source,
                token: request.token.clone(),
                failure: SourceFailure::Transport,
            });
        }
        if self.pending {
            self.pending = false;
            return Ok(SourceReply::Pending);
        }
        let reply = self.source.poll(request, work, cancel)?;
        if let SourceReply::Data(window) = &reply {
            self.next += window.bytes().len() as u64;
        }
        Ok(reply)
    }

    fn invalidate(&mut self) {
        self.calls.borrow_mut().push("invalidate");
        self.source.invalidate();
    }

    fn abort(&mut self) {
        self.calls.borrow_mut().push("abort");
        self.source.abort();
    }

    fn close(&mut self) {
        self.calls.borrow_mut().push("close");
        self.source.close();
    }
}

fn probe<'a>(
    pool: &Budget,
    bytes: &Bytes,
    calls: &'a RefCell<Vec<&'static str>>,
    fail: bool,
) -> Result<Probe<Sequential<'a>>> {
    let source = Sequential {
        source: ByteSource::new(ID, bytes.clone(), WINDOW)?,
        next: 0,
        pending: true,
        fail,
        calls,
    };
    let caps = SourceCapabilities {
        length: None,
        random_access: false,
    };
    Ok(Probe::new(pool, ID, caps, WINDOW, source)?)
}

fn next(probe: &mut Probe<Sequential<'_>>) -> triergon_core::Result<ProbeProgress> {
    probe.advance(&mut WorkBudget::new(WORK), &AtomicBool::new(false))
}

fn selected(
    pool: &Budget,
    data: &Bytes,
    calls: &RefCell<Vec<&'static str>>,
) -> Result<(u64, [u8; 16])> {
    let mut probe = probe(pool, data, calls, false)?;
    let cancel = AtomicBool::new(false);
    if !matches!(
        probe.advance(&mut WorkBudget::new(0), &cancel)?,
        ProbeProgress::Yield
    ) {
        return Err("probe spent work without a grant".into());
    }
    let ProbeProgress::NeedInput(request) = next(&mut probe)? else {
        return Err("probe selected before reading the source".into());
    };
    if request.source != ID || request.offset != 0 || request.maximum != WINDOW {
        return Err(format!("unexpected opening probe request: {request:?}").into());
    }
    loop {
        match next(&mut probe)? {
            ProbeProgress::Selected(Format::Matroska) => break,
            ProbeProgress::Yield | ProbeProgress::NeedInput(_) => {}
            other => return Err(format!("unexpected probe selection: {other:?}").into()),
        }
    }
    let mut selected = probe.finish()?;
    if selected.format != Format::Matroska || selected.input.position() != 0 {
        return Err("probe changed format or starting position at handoff".into());
    }
    selected.source.invalidate();
    read_replay(selected.input, selected.source, &cancel)
}

fn record(totals: &mut Totals, kind: u64, calls: &RefCell<Vec<&'static str>>, trace: bool) {
    let calls = calls.take();
    fold(&mut totals.digest, kind);
    for call in &calls {
        fold_bytes(&mut totals.digest, call.as_bytes());
    }
    totals.packets += 1;
    if trace {
        println!("{kind} {}", calls.join(" "));
    }
}

fn check(pool: &Budget, bytes: &Bytes, trace: bool) -> Result<Totals> {
    let mut totals = Totals {
        packets: 0,
        bytes: 0,
        digest: 0,
        refusal: None,
    };
    let calls = RefCell::new(Vec::new());
    if !matches!(
        probe(pool, bytes, &calls, false)?.finish(),
        Err(Error::Invalid("format not selected"))
    ) {
        return Err("unfinished probe did not refuse handoff".into());
    }
    record(&mut totals, 1, &calls, trace);
    let (length, hash) = selected(pool, bytes, &calls)?;
    record(&mut totals, 2, &calls, trace);
    fold(&mut totals.digest, length);
    fold_bytes(&mut totals.digest, &hash);
    totals.packets += 1;
    totals.bytes += hash.len() as u64;
    if trace {
        print!("5 {length} ");
        for byte in hash {
            print!("{byte:02x}");
        }
        println!();
    }
    let mut failed = probe(pool, bytes, &calls, true)?;
    if !matches!(next(&mut failed)?, ProbeProgress::NeedInput(_))
        || !matches!(next(&mut failed), Err(Error::Transport))
        || !matches!(next(&mut failed), Err(Error::Closed))
        || !matches!(failed.finish(), Err(Error::Closed))
    {
        return Err("probe did not retain the source failure".into());
    }
    record(&mut totals, 3, &calls, trace);
    let mut cancelled = probe(pool, bytes, &calls, false)?;
    if !matches!(
        cancelled.advance(&mut WorkBudget::new(WORK), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ) || !matches!(cancelled.finish(), Err(Error::Closed))
    {
        return Err("cancelled probe did not close the source".into());
    }
    record(&mut totals, 4, &calls, trace);
    Ok(totals)
}

pub(super) fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, "probe-lifecycle-", "", 0..=0, true)?;
    let input = invocation.input(MAX_INPUT)?;
    let pool = Budget::new(128 << 20)?;
    let bytes = Bytes::try_from_vec(&pool, input).map_err(|error| error.error)?;
    let trace = invocation.mode == Mode::Trace;
    measure(&invocation, "playback-probe-lifecycle", Some(&pool), || {
        check(&pool, &bytes, trace)
    })
}
