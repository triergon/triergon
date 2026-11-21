//! Seeks by scanning: authored files, a configurable source feed and the drivers
//! that open, seek and drain a reader, shared by the unit and integration tests.
//! Library names resolve at `crate::`: the library itself, or an integration test
//! that imports them at its root.
// Each test binary uses part of this file.
#![allow(dead_code)]
use crate::ebml_kit::{cluster, crc32, element as e, mkv, uint as u};
use crate::timing::BlockTiming;
use crate::{
    Event, InitializationOrigin, Limits, ParseStep, Reader, SeekReceipt, SourcePosition,
    TypedPacket,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, Result, SourceCapabilities, SourceFailure,
    SourceId, Step, WorkBudget,
};

pub(crate) fn entry(number: u64, kind: u64, extra: &[u8]) -> Vec<u8> {
    e(
        0xae,
        &[
            u(0xd7, number),
            u(0x73c5, number),
            u(0x83, kind),
            e(
                0x86,
                if kind == 17 {
                    b"S_TEXT/UTF8"
                } else {
                    b"V_AUTHORED/OPAQUE"
                },
            ),
            e(0x63a2, b"initial"),
            extra.to_vec(),
        ]
        .concat(),
    )
}

pub(crate) fn simple(number: u64, width: usize, offset: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
    assert!((1..=8).contains(&width) && number < (1_u64 << (7 * width)) - 1);
    let mut body = ((1_u64 << (7 * width)) | number).to_be_bytes()[8 - width..].to_vec();
    body.extend(offset.to_be_bytes());
    body.push(flags);
    body.extend(payload);
    e(0xa3, &body)
}

pub(crate) struct Fixture {
    pub(crate) raw: Vec<u8>,
    pub(crate) clusters: u64,
}

pub(crate) fn file(entries: &[u8], info: &[u8], clusters: &[Vec<u8>]) -> Fixture {
    let body = [info.to_vec(), e(0x1654ae6b, entries), clusters.concat()].concat();
    Fixture {
        raw: mkv(&body),
        // Independent concatenated wire lengths, not a parser/seek helper.
        clusters: clusters.iter().map(|bytes| bytes.len() as u64).sum(),
    }
}

pub(crate) fn repeated(count: usize, payload: usize) -> Fixture {
    let blocks: Vec<_> = (0..count)
        .map(|index| simple(1, 1, index as i16, 0x80, &vec![b'x'; payload]))
        .collect();
    file(&entry(1, 1, &[]), &[], &[cluster(0, blocks.concat())])
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub(crate) requested: i64,
    pub(crate) actual: i64,
    pub(crate) scanned: u64,
    pub(crate) indexed: bool,
    pub(crate) discard_until: i64,
    pub(crate) preroll: u64,
    pub(crate) from_start: bool,
}

impl From<SeekReceipt> for Receipt {
    fn from(value: SeekReceipt) -> Self {
        Self {
            requested: value.requested_ns,
            actual: value.actual_ns,
            scanned: value.scanned_bytes,
            indexed: value.used_index,
            discard_until: value.discard_until_ns,
            preroll: value.preroll_ns,
            from_start: value.replays_from_start,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TrackView {
    pub(crate) source: Vec<u8>,
    pub(crate) private: Vec<u8>,
    pub(crate) initialization: Vec<u8>,
    pub(crate) origin: InitializationOrigin,
    pub(crate) revision: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PacketView {
    pub(crate) track: usize,
    pub(crate) pts: Option<i64>,
    pub(crate) dts: Option<i64>,
    pub(crate) duration: Option<u64>,
    pub(crate) key: bool,
    pub(crate) padding: i64,
    pub(crate) timing: BlockTiming,
    pub(crate) payload: Vec<u8>,
    pub(crate) capacity: usize,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) tracks: Vec<TrackView>,
    pub(crate) group: Option<(Vec<u8>, Option<SourcePosition>)>,
}

pub(crate) fn view(value: TypedPacket) -> PacketView {
    let packet = &value.packet;
    let tracks = value
        .configuration
        .tracks()
        .iter()
        .enumerate()
        .map(|(at, track)| {
            let initialization = value.configuration.initialization(at).unwrap();
            TrackView {
                source: track.source_metadata.to_vec(),
                private: track.codec_private.to_vec(),
                initialization: initialization.data.to_vec(),
                origin: initialization.origin,
                revision: initialization.revision,
            }
        })
        .collect();
    PacketView {
        track: packet.track,
        pts: packet.pts_ns,
        dts: packet.dts_ns,
        duration: packet.duration_ns,
        key: packet.keyframe,
        padding: packet.discard_padding_ns,
        timing: packet.timing,
        payload: packet.data.as_ref().to_vec(),
        capacity: packet.data.retained_capacity(),
        generation: value.configuration.generation(),
        revision: value.configuration.revision(),
        tracks,
        group: value
            .group
            .as_ref()
            .map(|group| (group.raw_body().to_vec(), group.position().unwrap())),
    }
}

pub(crate) struct Feed<'a> {
    pub(crate) raw: &'a [u8],
    pub(crate) chunk: usize,
    pub(crate) hidden: bool,
    pub(crate) hold: bool,
    pub(crate) alias: Option<Bytes>,
    pub(crate) fault: Option<(usize, bool)>,
    pub(crate) immutable: Option<Bytes>,
    pub(crate) requests: usize,
}

impl<'a> Feed<'a> {
    pub(crate) fn new(raw: &'a [u8], chunk: usize) -> Self {
        Self {
            raw,
            chunk,
            hidden: false,
            hold: false,
            alias: None,
            fault: None,
            immutable: None,
            requests: 0,
        }
    }
    pub(crate) fn supply(&mut self, reader: &mut Reader, request: InputRequest) -> Result<()> {
        self.requests += 1;
        let at = usize::try_from(request.offset).unwrap();
        assert!(at <= self.raw.len() && (request.maximum != 0 || at == self.raw.len()));
        if let Some((end, transport)) = self.fault
            && at == end
        {
            return if transport {
                reader.fail(request.source, request.token, SourceFailure::Transport)
            } else {
                reader.finish(request.source, request.token)
            };
        }
        if at == self.raw.len() {
            return reader.finish(request.source, request.token);
        }
        let stop = self.fault.map_or(self.raw.len(), |(end, _)| end);
        let end = (at + request.maximum.min(self.chunk)).min(stop);
        assert!(end > at);
        let bytes = if let Some(root) = &self.immutable {
            root.slice(at..end)?
        } else if self.hidden {
            let mut parent = vec![0_u8; 512 + end - at];
            parent[16..16 + end - at].copy_from_slice(&self.raw[at..end]);
            Bytes::copy_from_slice(reader.budget(), &parent)?.slice(16..16 + end - at)?
        } else {
            Bytes::copy_from_slice(reader.budget(), &self.raw[at..end])?
        };
        if self.hold {
            self.alias = Some(bytes.clone());
        }
        reader.provide(&InputWindow::new(
            request.source,
            request.token,
            request.offset,
            bytes,
        )?)
    }
}

pub(crate) fn new_reader(pool: &Budget, raw: &[u8], limits: Limits) -> Reader {
    Reader::new(
        pool,
        SourceId(28001),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        limits,
        8192,
    )
    .unwrap()
}

pub(crate) fn open(reader: &mut Reader, feed: &mut Feed<'_>) -> Result<()> {
    for _ in 0..200_000 {
        if reader.is_ready() {
            return Ok(());
        }
        match reader.step_parse(&mut WorkBudget::new(8192), &AtomicBool::new(false))? {
            ParseStep::NeedInput(request) => feed.supply(reader, request)?,
            ParseStep::Yield | ParseStep::Event(_) => (),
            other => panic!("opening must be bounded: {other:?}"),
        }
    }
    panic!("opening stalled")
}

pub(crate) fn drain(
    reader: &mut Reader,
    feed: &mut Feed<'_>,
    quantum: u64,
) -> Result<Vec<PacketView>> {
    let mut packets = Vec::new();
    for _ in 0..2_000_000 {
        let mut work = WorkBudget::new(quantum);
        let step = reader.step(&mut work, &AtomicBool::new(false))?;
        assert!(work.spent() <= quantum);
        match step {
            Step::NeedInput(request) => feed.supply(reader, request)?,
            Step::Ready(Event::Packet(value)) => packets.push(view(value)),
            Step::Yield | Step::Ready(_) => (),
            Step::End(_) => return Ok(packets),
            other @ Step::Backpressure(_) => panic!("ample-pool replay stalled: {other:?}"),
        }
    }
    panic!("replay stalled")
}

pub(crate) fn seek(
    reader: &mut Reader,
    feed: &mut Feed<'_>,
    quantum: u64,
) -> Result<(Receipt, u64)> {
    let mut spent = 0;
    for _ in 0..2_000_000 {
        let mut work = WorkBudget::new(quantum);
        let step = reader.step(&mut work, &AtomicBool::new(false));
        assert!(work.spent() <= quantum);
        spent += work.spent();
        match step? {
            Step::NeedInput(request) => feed.supply(reader, request)?,
            Step::Ready(Event::Seek(receipt)) => return Ok((receipt.into(), spent)),
            Step::Ready(Event::Packet(_)) => {
                panic!("no packet before a completed seek receipt")
            }
            Step::Yield | Step::Ready(_) => (),
            other => panic!("seek must finish or report its real error: {other:?}"),
        }
    }
    panic!("seek stalled")
}

pub(crate) fn seek_error(fixture: &Fixture, fault: Option<(usize, bool)>, chunk: usize) -> Error {
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, chunk);
    open(&mut reader, &mut feed).unwrap();
    feed.fault = fault;
    reader.seek_for_playback(0, 0).unwrap();
    let error =
        seek(&mut reader, &mut feed, 8192).expect_err("invalid source must not emit a receipt");
    assert!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .is_err()
    );
    drop((reader, feed));
    assert_eq!(pool.used(), baseline);
    error
}

pub(crate) fn group(corrupt: bool) -> Vec<u8> {
    let body = [
        e(0xa1, &[0x81, 0, 0, 0, b'g']),
        e(0xa4, b"new-state"),
        e(
            0x75a1,
            &e(0xa6, &[u(0xee, 1), e(0xa5, b"opaque-addition")].concat()),
        ),
    ]
    .concat();
    let checksum = crc32(&body) ^ u32::from(corrupt);
    e(0xa0, &[e(0xbf, &checksum.to_le_bytes()), body].concat())
}

pub(crate) fn immutable_fixture(raw: &[u8], limits: Limits) -> (Reader, Feed<'_>) {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let root = Bytes::copy_from_slice(&pool, raw).unwrap();
    let reader =
        Reader::new_with_immutable_seek_index(&pool, SourceId(28001), root.clone(), limits, 8192)
            .unwrap();
    let mut feed = Feed::new(raw, 7);
    feed.immutable = Some(root);
    (reader, feed)
}
