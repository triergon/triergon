use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error as C, InputRequest, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_playback::{
    ByteSource, Source, SourceReply,
    mp4::{Mp4Event, Mp4Packet},
};
use triergon_playback::{
    mp4::{Mp4IndexJob, Mp4Open, OpenLimits, OpenProgress, PlanLimits},
    *,
};

const ID: SourceId = SourceId(771);

fn pool() -> Budget {
    Budget::new(64 * 1024 * 1024).unwrap()
}

fn mp4_packets(v: &[Output]) -> Vec<&Mp4Packet> {
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Poll(u64),
    Invalidate,
    Abort,
    Close,
}

struct Observed {
    inner: ByteSource,
    calls: Arc<Mutex<Vec<Call>>>,
    fail: Arc<AtomicBool>,
    end: Option<(u64, Arc<AtomicBool>)>,
    sequential: bool,
    next: u64,
    pending: usize,
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

type Calls = Arc<Mutex<Vec<Call>>>;

fn observed(
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

fn off() -> AtomicBool {
    AtomicBool::new(false)
}

/// Advance a player to its end, `units` at a time, and return what it delivered.
fn collect<S: Source>(player: &mut Player<S>, units: u64) -> Result<Vec<Output>> {
    let mut out = Vec::new();
    for _ in 0..2_000_000 {
        let progress = player.advance(&mut WorkBudget::new(units), &off())?;
        out.extend(std::iter::from_fn(|| player.pop()));
        if matches!(progress, Progress::Ended) {
            return Ok(out);
        }
    }
    panic!("player did not make bounded progress");
}

#[test]
fn combined_playback_probe_opens_real_sequential_mp4() {
    use triergon_framing::nal::RawAnnexBLimits;
    use triergon_raw_video::{
        PlaybackInput, PlaybackInputKind, PlaybackProbe, PlaybackProbeStep, RawVideoProbeLimits,
    };

    let raw = include_bytes!("../../../playback/tests/fixtures/mp4/r-front-stco.mov");
    let b = pool();
    let (source, calls, _) = observed(&b, raw, 7, true);
    let mut probe = PlaybackProbe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        4096,
        source,
        RawVideoProbeLimits::default(),
    )
    .unwrap();
    let detected =
        (0..1000).find_map(
            |_| match probe.advance(&mut WorkBudget::new(1), &off()).unwrap() {
                PlaybackProbeStep::Detected(kind) => Some(kind),
                PlaybackProbeStep::Yield | PlaybackProbeStep::NeedInput(_) => None,
            },
        );
    assert_eq!(detected, Some(PlaybackInputKind::Container(Format::Mp4)));
    let PlaybackInput::Container(selected) = probe.finish(RawAnnexBLimits::default()).unwrap()
    else {
        panic!("combined probe did not hand off MP4");
    };
    drop(probe);
    let mut open = Mp4Open::new(selected, OpenLimits::default()).unwrap();
    let ready = (0..100_000).any(|_| {
        matches!(
            open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            OpenProgress::Ready
        )
    });
    assert!(ready);
    let opened = open.finish().unwrap();
    let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
    assert!((0..100_000).any(|_| index.poll(&mut WorkBudget::new(1), &off()).unwrap()));
    let mut player = Player::new(
        Engine::mp4(opened.input, index.finish().unwrap(), 1 << 20).unwrap(),
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    drop(opened.movie);
    let out = collect(&mut player, 1).unwrap();
    let packets = mp4_packets(&out);
    assert_eq!(packets.len(), 6);
    for packet in packets {
        let range = &packet.source.sample.range;
        assert_eq!(
            packet.source.payload.as_ref(),
            &raw[range.start as usize..range.end as usize]
        );
    }
    let calls = calls.lock().unwrap();
    let offsets: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            Call::Poll(offset) => Some(*offset),
            _ => None,
        })
        .collect();
    assert!(offsets.windows(2).all(|offsets| offsets[0] < offsets[1]));
    assert!(!calls.contains(&Call::Invalidate));
    drop(calls);
    drop(out);
    drop(player);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
