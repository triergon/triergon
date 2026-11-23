//! Direct exercises of byte-supply and blocking reader contracts.

use std::{
    ffi::OsString,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
};

use triergon::elementary::{AdtsReader, Limits};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, Time, WorkBudget,
    blocking::{Blocking, Feed, seekable_capabilities},
    container::{ContainerReader, Event, PacketInfo, SeekPolicy, SeekRequest, TrackInfo},
};
use triergon_harness_protocol::hex;

use triergon_harness_runner::{Operation, Totals};

const SOURCE: SourceId = SourceId(1);
const NEXT: SourceId = SourceId(2);
static READY: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(true);

fn caps(length: Option<u64>, random_access: bool) -> SourceCapabilities {
    SourceCapabilities {
        length,
        random_access,
    }
}

fn row(
    trace: &mut Option<&mut dyn Write>,
    work: &mut Totals,
    kind: u64,
    values: &[u64],
    bytes: &[u8],
) -> Result<(), String> {
    work.record(kind, values, bytes);
    if let Some(out) = trace {
        write!(out, "{kind}")
            .and_then(|()| {
                for value in values {
                    write!(out, " {value}")?;
                }
                if !bytes.is_empty() {
                    write!(out, " {}", hex(bytes))?;
                }
                writeln!(out)
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn window(
    pool: &Budget,
    request: &InputRequest,
    bytes: &[u8],
) -> triergon_core::Result<InputWindow> {
    InputWindow::new(
        request.source,
        request.token.clone(),
        request.offset,
        Bytes::copy_from_slice(pool, bytes)?,
    )
}

fn require(condition: bool, what: &'static str) -> triergon_core::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Invalid(what))
    }
}

fn checked_window(
    reader: &mut InputCursor,
    input: &[u8],
) -> triergon_core::Result<(InputRequest, Bytes)> {
    let pool = reader.budget().clone();
    let first = reader.request();
    require(
        reader.source() == SOURCE
            && first.source == SOURCE
            && first.offset == 0
            && first.token.request() == 0
            && first.token.generation() == reader.generation(),
        "initial cursor source and token",
    )?;
    require(
        reader
            .pending_request(&READY)?
            .is_some_and(|r| r.token == first.token),
        "initial pending request",
    )?;
    reader.provide(&window(&pool, &first, &[])?)?;
    require(
        matches!(reader.read_discard(1, &mut WorkBudget::new(2), &READY)?,
                   Step::NeedInput(request) if request.token == first.token && request.offset == 0),
        "empty response must starve",
    )?;
    let foreign = Budget::new(1 << 20)?;
    let same_request_wrong_pool = window(&foreign, &first, &input[..1])?;
    let mut wrong_source = window(&pool, &first, &input[..1])?;
    wrong_source.source = NEXT;
    let mut wrong_offset = window(&pool, &first, &input[..1])?;
    wrong_offset.offset = 1;
    require(
        reader.provide(&same_request_wrong_pool) == Err(Error::ForeignPool)
            && reader.provide(&wrong_source) == Err(Error::SourceMismatch)
            && matches!(reader.provide(&wrong_offset), Err(Error::Invalid(_)))
            && matches!(
                reader.provide(&window(&pool, &first, &input[..9])?),
                Err(Error::Limit { .. })
            )
            && reader.position() == 0,
        "refused windows move the cursor",
    )?;
    reader.provide_owned(window(&pool, &first, &input[..8])?)?;
    let owner = reader
        .buffered_owner()
        .ok_or(Error::Invalid("no supplied owner"))?
        .clone();
    require(
        reader.pending_request(&READY)?.is_none()
            && reader.buffered() == &input[..8]
            && matches!(
                reader.provide_inline(SOURCE, first.token.clone(), 0, &input[..1]),
                Err(Error::Invalid(_))
            ),
        "buffered window can be replaced",
    )?;
    Ok((first, owner))
}

fn check_sequential(reader: &mut InputCursor, last: &InputRequest) -> triergon_core::Result<()> {
    let next = reader.continue_source(NEXT, caps(None, false))?;
    require(
        next.source == NEXT
            && next.offset == 0
            && next.token.request() == 3
            && next.token.generation() == last.token.generation(),
        "continuation reset the generation or wrong source",
    )?;
    require(
        matches!(
            reader.continue_source(SOURCE, caps(None, false)),
            Err(Error::Invalid(_))
        ) && matches!(reader.request_range(1), Err(Error::Unsupported(_)))
            && matches!(reader.reset(1), Err(Error::Unsupported(_))),
        "sequential source allowed a jump or premature handoff",
    )?;
    let mut budget = WorkBudget::new(2);
    require(
        !reader.consume_response(&next, 3, &mut budget, &READY, || Err(Error::Transport))?
            && reader.consume_response(&next, 2, &mut budget, &READY, || Err(Error::Transport))
                == Err(Error::Transport)
            && reader.position() == 0
            && budget.remaining() == 2
            && reader.consume_response(&next, 2, &mut budget, &READY, || Ok(()))?
            && reader.position() == 2
            && budget.remaining() == 0,
        "direct response consumption was not atomic",
    )?;
    for (failure, error) in [
        (Some(SourceFailure::Transport), Error::Transport),
        (Some(SourceFailure::Cancelled), Error::Cancelled),
        (None, Error::Cancelled),
    ] {
        let before = reader.request();
        if let Some(failure) = failure {
            reader.fail(NEXT, before.token.clone(), failure)?;
        } else if !matches!(reader.pending_request(&CANCEL), Err(Error::Cancelled)) {
            return Err(Error::Invalid("cancellation not observed"));
        }
        require(
            matches!(reader.read_discard(1, &mut WorkBudget::new(1), &READY), Err(e) if e == error),
            "source failure not sticky",
        )?;
        let restarted = reader.reset(0)?;
        require(
            restarted.token.generation() != before.token.generation()
                && restarted.token.request() == 0
                && matches!(
                    reader.fail(NEXT, before.token, SourceFailure::Transport),
                    Err(Error::StaleGeneration { .. })
                ),
            "old generation accepted or not replaced",
        )?;
    }
    let final_request = reader.request();
    reader.finish(NEXT, final_request.token)?;
    require(
        reader.capabilities() == caps(Some(0), false),
        "finish did not establish length",
    )?;

    let mut short = InputCursor::new(reader.budget(), SOURCE, caps(Some(1), false), 8)?;
    require(
        short.finish(SOURCE, short.token()) == Err(Error::Truncated)
            && matches!(short.pending_request(&READY), Err(Error::Truncated)),
        "known-length truncation must be sticky",
    )?;
    Ok(())
}

fn cursor(
    input: &[u8],
    trace: &mut Option<&mut dyn Write>,
    work: &mut Totals,
) -> Result<(), String> {
    let pool = Budget::new(1 << 20).map_err(|e| e.to_string())?;
    let mut reader = InputCursor::new(&pool, SOURCE, caps(Some(input.len() as u64), true), 8)
        .map_err(|e| e.to_string())?;
    let controls = (|| -> triergon_core::Result<_> {
        let (first, owner) = checked_window(&mut reader, input)?;
        let mut copied = [0; 4];
        let mut budget = WorkBudget::new(2);
        require(
            matches!(
                reader.read_into(&mut copied, &mut budget, &READY)?,
                Step::Ready(2)
            ) && matches!(reader.read_discard(1, &mut budget, &READY)?, Step::Yield)
                && reader.position() == 2
                && budget.remaining() == 0,
            "partial read and work yield",
        )?;
        let copied_end = reader.position();
        let reused = reader.request_range_reusing_window(1)?;
        let span_start = reader.position();
        require(
            reused.token.request() == 1
                && reused.token.generation() == first.token.generation()
                && matches!(
                    reader.provide(&window(&pool, &first, &input[..1])?),
                    Err(Error::StaleRequest { .. })
                ),
            "range token and identity",
        )?;
        let Step::Ready(span) = reader.read_span(2, &mut WorkBudget::new(2), &READY)? else {
            return Err(Error::Invalid("span was not ready"));
        };
        require(
            std::ptr::eq(span.as_ptr(), owner.as_ptr().wrapping_add(1)),
            "span does not retain the original owned window",
        )?;

        let span_end = reader.position();
        let output_pool = Budget::new(128)?;
        let mut accumulated = BudgetVec::new(&output_pool);
        let held = output_pool.reserve(output_pool.available())?;
        let mut budget = WorkBudget::new(3);
        require(
            matches!(
                reader.read_append(&mut accumulated, 3, &mut budget, &READY),
                Err(Error::Backpressure(_))
            ) && reader.position() == 3
                && budget.remaining() == 3
                && accumulated.is_empty(),
            "append consumed cursor or work under backpressure",
        )?;
        drop(held);
        require(
            matches!(
                reader.read_append(&mut accumulated, 3, &mut budget, &READY)?,
                Step::Ready(3)
            ) && reader.position() == 6
                && budget.remaining() == 0,
            "append retry did not consume the bytes",
        )?;
        let appended_end = reader.position();
        require(
            matches!(
                reader.read_discard(2, &mut WorkBudget::new(2), &READY)?,
                Step::Ready(2)
            ),
            "discard did not reach the end of a window",
        )?;

        let discarded_end = reader.position();
        let last = reader.request_range(input.len() as u64 - 1)?;
        require(
            last.maximum == 1 && last.token.request() == 2,
            "known length did not clip request",
        )?;
        reader.provide_inline(
            SOURCE,
            last.token.clone(),
            last.offset,
            &input[input.len() - 1..],
        )?;
        require(
            reader.buffered_owner().is_none(),
            "inline input has an allocation",
        )?;
        reader.finish(SOURCE, last.token.clone())?;
        let Step::Ready(inline) = reader.read_span(1, &mut WorkBudget::new(1), &READY)? else {
            return Err(Error::Invalid("final inline byte was not available"));
        };
        let Step::End(end) = reader.read_discard(1, &mut WorkBudget::new(1), &READY)? else {
            return Err(Error::Invalid("explicit end not reported"));
        };
        check_sequential(&mut reader, &last)?;

        let offsets = [
            copied_end,
            span_start,
            span_end,
            appended_end,
            discarded_end,
        ];
        Ok((copied, span, accumulated, inline, end, offsets))
    })()
    .map_err(|e| e.to_string())?;
    let (copied, span, accumulated, inline, end, offsets) = controls;
    let [
        copied_end,
        span_start,
        span_end,
        appended_end,
        discarded_end,
    ] = offsets;
    row(
        trace,
        work,
        1,
        &[copied_end, span_end, appended_end, discarded_end],
        &copied[..2],
    )?;
    row(trace, work, 2, &[span_start, span_end], span.as_ref())?;
    row(
        trace,
        work,
        3,
        &[span_end, appended_end],
        accumulated.as_slice(),
    )?;
    row(
        trace,
        work,
        4,
        &[
            end.source.0,
            end.offset,
            reader.source().0,
            reader.position(),
        ],
        inline.as_ref(),
    )
}

struct Source<'a> {
    inner: Cursor<&'a [u8]>,
    seeks: usize,
    interrupt: bool,
    broken: bool,
}

impl<'a> Source<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            inner: Cursor::new(input),
            seeks: 0,
            interrupt: true,
            broken: false,
        }
    }
}

impl Read for Source<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.broken {
            return Err(io::Error::other("test source failure"));
        }
        if std::mem::take(&mut self.interrupt) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.inner.read(output)
    }
}

impl Seek for Source<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.seeks += 1;
        self.inner.seek(position)
    }
}

fn feed(input: &[u8], trace: &mut Option<&mut dyn Write>, work: &mut Totals) -> Result<(), String> {
    for maximum in [1, 8, 64, 8192] {
        let observations = (|| -> triergon_core::Result<_> {
            let pool = Budget::new(1 << 20)?;
            let mut reader =
                InputCursor::new(&pool, SOURCE, caps(Some(input.len() as u64), true), maximum)?;
            let mut feed = Feed::new(Source::new(input), &pool)?;
            let mut data = Vec::new();
            let end = loop {
                let mut bytes = [0; 64];
                match reader.read_into(&mut bytes, &mut WorkBudget::new(64), &READY)? {
                    Step::Ready(count) => data.extend_from_slice(&bytes[..count]),
                    Step::NeedInput(request) => feed.supply(&mut reader, request)?,
                    Step::End(end) => break end,
                    Step::Yield => return Err(Error::Invalid("feed yielded with enough work")),
                }
            };
            let linear = feed.source_mut().seeks;
            let request = reader.request_range(2)?;
            feed.supply(&mut reader, request)?;
            let repositioned = feed.source_mut().seeks;
            let reread = reader.buffered().to_vec();
            feed.source_mut().broken = true;
            let request = reader.request_range(0)?;
            require(
                feed.supply(&mut reader, request) == Err(Error::Io(io::ErrorKind::Other))
                    && matches!(reader.pending_request(&READY), Err(Error::Transport)),
                "feed failed without poisoning the cursor",
            )?;
            Ok((data, end, linear, repositioned, reread))
        })()
        .map_err(|e| e.to_string())?;
        let (data, end, linear, repositioned, reread) = observations;
        row(
            trace,
            work,
            5,
            &[maximum as u64, end.source.0, end.offset, linear as u64],
            &data,
        )?;
        row(
            trace,
            work,
            6,
            &[maximum as u64, repositioned as u64],
            &reread,
        )?;
    }
    Ok(())
}

fn blocking(
    input: &[u8],
    trace: &mut Option<&mut dyn Write>,
    work: &mut Totals,
) -> Result<(), String> {
    for random_access in [false, true] {
        let mut source = Source::new(input);
        let pool = Budget::new(1 << 20).map_err(|e| e.to_string())?;
        let caps = if random_access {
            seekable_capabilities(&mut source).map_err(|e| e.to_string())?
        } else {
            caps(None, false)
        };
        let reader = AdtsReader::new(&pool, SOURCE, caps, Limits::default(), 127)
            .map_err(|e| e.to_string())?;
        let mut driver = if random_access {
            Blocking::seekable(reader, source, &pool)
        } else {
            Blocking::sequential(reader, source, &pool)
        };
        let held = pool.reserve(pool.available()).map_err(|e| e.to_string())?;
        if !matches!(driver.next_event(), Err(Error::Backpressure(_))) {
            return Err("the blocking driver did not report the occupied pool".into());
        }
        drop(held);
        while let Some(packet) = driver.next_packet().map_err(|e| e.to_string())? {
            let track = driver
                .reader()
                .track(0)
                .ok_or("no track for an ADTS packet")?;
            row(
                trace,
                work,
                7,
                &[
                    u64::from(random_access),
                    track.id().0,
                    packet.pts().ok_or("ADTS clock absent")? as u64,
                    packet.duration().ok_or("ADTS duration absent")? as u64,
                ],
                packet.payload(),
            )?;
        }
        row(
            trace,
            work,
            8,
            &[
                u64::from(random_access),
                driver.reader().track_count() as u64,
                driver.into_inner().inner.position(),
            ],
            &[],
        )?;
        if random_access {
            blocking_seek(input, &pool, caps, trace, work)?;
        }
    }
    let pool = Budget::new(1 << 20).map_err(|e| e.to_string())?;
    let reader = AdtsReader::new(&pool, SOURCE, caps(None, false), Limits::default(), 127)
        .map_err(|e| e.to_string())?;
    let mut source = Source::new(input);
    source.broken = true;
    let mut driver = Blocking::sequential(reader, source, &pool);
    if !matches!(driver.next_event(), Err(Error::Io(io::ErrorKind::Other)))
        || !matches!(driver.next_event(), Err(Error::Transport))
    {
        return Err("blocking I/O failure was not sticky".into());
    }
    let reader = AdtsReader::new(&pool, SOURCE, caps(None, false), Limits::default(), 127)
        .map_err(|e| e.to_string())?;
    let mut driver = Blocking::sequential(reader, Source::new(input), &pool);
    if !matches!(driver.next_event_or_cancel(&CANCEL), Err(Error::Cancelled))
        || !matches!(driver.next_event(), Err(Error::Cancelled))
    {
        return Err("blocking cancellation was not sticky".into());
    }
    row(trace, work, 9, &[driver.into_inner().inner.position()], &[])
}

fn blocking_seek(
    input: &[u8],
    pool: &Budget,
    caps: SourceCapabilities,
    trace: &mut Option<&mut dyn Write>,
    work: &mut Totals,
) -> Result<(), String> {
    let reader =
        AdtsReader::new(pool, SOURCE, caps, Limits::default(), 7).map_err(|e| e.to_string())?;
    let mut seeking = Blocking::seekable(reader, Source::new(input), pool);
    let first = seeking
        .next_packet()
        .map_err(|e| e.to_string())?
        .ok_or("missing initial packet")?;
    let zero = Time::new(0, 1).map_err(|e| e.to_string())?;
    seeking
        .begin_seek(SeekRequest {
            track: first.track(),
            target: zero,
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .map_err(|e| e.to_string())?;
    let receipt = loop {
        match seeking.next_event().map_err(|e| e.to_string())? {
            Some(Event::Seek(receipt)) => break receipt,
            Some(Event::Other(_)) => {}
            other => return Err(format!("expected restart seek receipt, got {other:?}")),
        }
    };
    let packet = seeking
        .next_packet()
        .map_err(|e| e.to_string())?
        .ok_or("missing seek packet")?;
    if receipt.landing != zero || receipt.requested != zero {
        return Err("the reader did not restart at time zero".into());
    }
    row(
        trace,
        work,
        10,
        &[
            packet.track().0,
            packet.pts().ok_or("missing restart timestamp")? as u64,
        ],
        packet.payload(),
    )
}

pub(super) fn operation<'a>(
    _prefix: &str,
    input: &'a [u8],
    args: &[OsString],
) -> Result<Operation<'a>, String> {
    if input.len() < 9 {
        return Err("reader-protocol needs nine source bytes".into());
    }
    let action = match args {
        [mode] if mode == "cursor" => cursor,
        [mode] if mode == "feed" => feed,
        [mode] if mode == "blocking" => blocking,
        _ => return Err("reader-protocol needs cursor, feed or blocking".into()),
    };
    Ok(Operation {
        name: "core-reader-protocol",
        run: Box::new(move |work, mut trace| action(input, &mut trace, work)),
    })
}
