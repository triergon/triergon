//! Players over the fixture inputs: MP4, TS and probed, and the observed sources that
//! record what a player asks of them.
use super::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error as C, InputCursor, InputRequest, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{PacketFormat, clock::SourceTimePolicy};
use triergon_playback::{
    ByteSource, Source, SourceReply,
    mp4::{Mp4Event, Mp4Packet},
};
use triergon_playback::{
    SourceOwner,
    mp4::{Mp4Open, OpenLimits, PlanLimits},
    probe::{Probe, ProbeProgress, SelectedInput},
    *,
};

pub(crate) const ID: SourceId = SourceId(771);

pub(crate) fn pool() -> Budget {
    Budget::new(64 * 1024 * 1024).unwrap()
}

pub(crate) fn open(
    raw: &[u8],
    quantum: usize,
    units: u64,
) -> Result<(mp4::OpenedMp4<ByteSource>, Budget)> {
    let b = pool();
    let capabilities = SourceCapabilities {
        length: Some(raw.len() as u64),
        random_access: true,
    };
    let mut job = Mp4Open::new(
        byte_input(&b, ID, raw, quantum, capabilities, 4096)?,
        OpenLimits {
            metadata_bytes: 1 << 20,
            ..OpenLimits::default()
        },
    )?;
    run_open(&mut job, units)?;
    Ok((job.finish()?, b))
}

pub(crate) fn mp4(
    raw: &[u8],
    view: View,
    quantum: usize,
    units: u64,
) -> Result<(Player<ByteSource>, Budget)> {
    let (opened, b) = open(raw, quantum, units)?;
    let mut job = index_job(&opened.movie, view, PlanLimits::default())?;
    for _ in 0..2_000_000 {
        if job.poll(&mut WorkBudget::new(units), &off())? {
            let plan = job.finish()?;
            let player = Player::new(
                Engine::mp4(opened.input, plan, 1 << 20)?,
                opened.source,
                Selection::All,
                PlayerPolicy::default(),
            )?;
            return Ok((player, b));
        }
    }
    panic!("real MP4 index did not finish");
}

pub(crate) fn ts(
    raw: &[u8],
    format: PacketFormat,
    quantum: usize,
    policy: SourceTimePolicy,
) -> Result<(Player<ByteSource>, Budget)> {
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        4096,
    )?;
    let source = SourceOwner::new(ByteSource::new(
        ID,
        Bytes::copy_from_slice(&b, raw)?,
        quantum,
    )?);
    Ok((
        Player::new(
            Engine::ts(
                input,
                format,
                1,
                triergon_mpegts::ParseLimits::default(),
                policy,
                triergon_mpegts::psi::DescriptorNamespace::Unspecified,
            )?,
            source,
            Selection::All,
            PlayerPolicy::default(),
        )?,
        b,
    ))
}

pub(crate) fn first_packet<S: Source>(p: &mut Player<S>) -> Output {
    for _ in 0..2_000_000 {
        p.advance(&mut WorkBudget::new(17), &off()).unwrap();
        while let Some(o) = p.pop() {
            if o.event().track().is_some() {
                return o;
            }
        }
    }
    panic!("no source packet");
}

pub(crate) fn mp4_packets(v: &[Output]) -> Vec<&Mp4Packet> {
    v.iter()
        .filter_map(|o| {
            if let Event::Mp4(Mp4Event::Packet(p)) = o.event() {
                Some(p)
            } else {
                None
            }
        })
        .collect()
}

pub(crate) fn pes(v: &[Output]) -> Vec<&TsEvent> {
    v.iter()
        .filter_map(|o| {
            if let Event::Ts(e) = o.event()
                && matches!(e.source, TraceEvent::Pes(_))
            {
                return Some(e);
            }
            None
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Poll(u64),
    Invalidate,
    Abort,
    Close,
}

pub(crate) struct Observed {
    pub(crate) inner: ByteSource,
    pub(crate) calls: Arc<Mutex<Vec<Call>>>,
    pub(crate) fail: Arc<AtomicBool>,
    pub(crate) end: Option<(u64, Arc<AtomicBool>)>,
    pub(crate) sequential: bool,
    pub(crate) next: u64,
    pub(crate) pending: usize,
}

impl Source for Observed {
    fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.calls.lock().unwrap().push(Call::Poll(r.offset));
        if self.pending > 0 {
            self.pending -= 1;
            return Ok(SourceReply::Pending);
        }
        if self.fail.load(Ordering::Relaxed) {
            return Ok(SourceReply::Failed {
                source: r.source,
                token: r.token.clone(),
                failure: triergon_core::SourceFailure::Transport,
            });
        }
        if self.sequential && r.offset != self.next {
            return Err(C::Invalid(
                "test sequential source was asked to rewind or skip",
            ));
        }
        if let Some((at, release)) = &self.end
            && r.offset == *at
            && !release.load(Ordering::Relaxed)
        {
            return Ok(SourceReply::Pending);
        }
        let out = self.inner.poll(r, w, c)?;
        if let SourceReply::Data(win) = &out {
            self.next = win.offset + win.bytes().len() as u64;
        }
        Ok(out)
    }
    fn invalidate(&mut self) {
        self.calls.lock().unwrap().push(Call::Invalidate);
        self.inner.invalidate();
    }
    fn abort(&mut self) {
        self.calls.lock().unwrap().push(Call::Abort);
        self.inner.abort();
    }
    fn close(&mut self) {
        self.calls.lock().unwrap().push(Call::Close);
        self.inner.close();
    }
}

pub(crate) type Calls = Arc<Mutex<Vec<Call>>>;

pub(crate) fn observed(
    b: &Budget,
    raw: &[u8],
    quantum: usize,
    sequential: bool,
) -> (Observed, Calls, Arc<AtomicBool>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(false));
    (
        Observed {
            inner: ByteSource::new(ID, Bytes::copy_from_slice(b, raw).unwrap(), quantum).unwrap(),
            calls: calls.clone(),
            fail: fail.clone(),
            end: None,
            sequential,
            next: 0,
            pending: 0,
        },
        calls,
        fail,
    )
}

pub(crate) fn probe(
    raw: &[u8],
    q: usize,
    sequential: bool,
) -> (SelectedInput<probe::ReplaySource<Observed>>, Budget, Calls) {
    let b = pool();
    let (source, calls, _) = observed(&b, raw, q, sequential);
    let mut probe = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: !sequential,
        },
        4096,
        source,
    )
    .unwrap();
    for _ in 0..2_000_000 {
        if let ProbeProgress::Selected(_) = probe.advance(&mut WorkBudget::new(1), &off()).unwrap()
        {
            return (probe.finish().unwrap(), b, calls);
        }
    }
    panic!("probe stalled");
}

pub(crate) fn observed_ts() -> (Player<Observed>, Budget, Calls, Arc<AtomicBool>) {
    let raw = include_bytes!("../../fixtures/ts/ts188-basic.ts");
    let b = pool();
    let (source, calls, fail) = observed(&b, raw, 7, false);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let p = Player::new(
        Engine::ts(
            input,
            PacketFormat::Ts188,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::RelativeEpochZero,
            triergon_mpegts::psi::DescriptorNamespace::Unspecified,
        )
        .unwrap(),
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    (p, b, calls, fail)
}

pub(crate) fn open_with_timing_policy(
    bytes: &[u8],
    policy: triergon_isobmff::table::TimingRunPolicy,
    random_access: bool,
) -> Result<(mp4::OpenedMp4<Observed>, Calls, Budget)> {
    let pool = pool();
    let (source, calls, _) = observed(&pool, bytes, 23, !random_access);
    let input = InputCursor::new(
        &pool,
        ID,
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access,
        },
        4096,
    )?;
    let mut opener = Mp4Open::with_timing_policy(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
        policy,
    )?;
    run_open(&mut opener, 17)?;
    Ok((opener.finish()?, calls, pool))
}

pub(crate) fn range_configs(
    track: &mp4::MovieTrack,
    major_brand: Option<[u8; 4]>,
    input: &mut InputCursor,
) -> Vec<triergon_isobmff::compat::mov::encoded::EncodedStream> {
    use triergon_isobmff::{
        CursorStatus,
        compat::mov::encoded::{EncodedSelection, EncodedStreamJob},
    };
    let cancel = off();
    track
        .table()
        .descriptions()
        .iter()
        .map(|description| {
            let mut stream = EncodedStreamJob::new_range_playback(
                track.header(),
                track.table(),
                input,
                EncodedSelection {
                    major_brand,
                    description: description.index,
                    limits: triergon_isobmff::config::ConfigurationLimits::default(),
                },
            )
            .unwrap();
            assert!((0..1000).any(|_| {
                stream
                    .poll(input, &mut WorkBudget::new(4096), &cancel)
                    .unwrap()
                    == CursorStatus::Ready
            }));
            stream.take(input, &cancel).unwrap()
        })
        .collect()
}

pub(crate) fn encoded_plans<S: triergon_playback::Source>(
    opened: &mut mp4::OpenedMp4<S>,
) -> Vec<Option<triergon_isobmff::compat::mov::edits::EncodedEditPlan>> {
    use triergon_isobmff::{
        CursorStatus,
        compat::mov::edits::{EncodedEditContext, EncodedEditJob},
    };
    let cancel = off();
    let mut plans = Vec::new();
    for track in opened.movie.tracks() {
        if triergon_isobmff::media_kind(&track.header().handler)
            == triergon_core::container::MediaKind::Data
        {
            plans.push(None);
            continue;
        }
        let Some(edits) = track.edits() else {
            plans.push(None);
            continue;
        };
        let configs = range_configs(track, opened.movie.major_brand(), &mut opened.input);
        let mut edit = EncodedEditJob::new_range_playback(
            EncodedEditContext {
                table: track.table(),
                edits,
                stream: &configs[0],
                media_base: track.header().time_base().unwrap(),
                movie_base: opened.movie.header().time_base().unwrap(),
            },
            &configs,
            &opened.input,
            triergon_isobmff::compat::mov::edits::EncodedEditLimits::default(),
        )
        .unwrap();
        while edit
            .poll(&mut opened.input, &mut WorkBudget::new(4096), &cancel)
            .unwrap()
            != CursorStatus::Ready
        {}
        plans.push(Some(edit.take(&mut opened.input, &cancel).unwrap()));
    }
    plans
}
