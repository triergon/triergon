//! An observed sequential source and the player over it that the progressive, pump
//! and bridge tests share.
use super::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_playback::{ByteSource, Source, SourceReply};
use triergon_playback::{
    SourceOwner,
    mp4::{Mp4IndexJob, Mp4Open, OpenLimits, PlanLimits, ProgressiveLimits},
    probe::SelectedInput,
    *,
};

pub(crate) const ID: SourceId = SourceId(901);

pub(crate) fn pool() -> Budget {
    Budget::new(64 << 20).unwrap()
}

pub(crate) struct Observed {
    pub(crate) source: ByteSource,
    pub(crate) gate: Arc<AtomicUsize>,
    pub(crate) calls: Arc<Mutex<Vec<&'static str>>>,
}

impl Source for Observed {
    fn poll(
        &mut self,
        r: &triergon_core::InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.calls.lock().unwrap().push("read");
        if r.offset >= self.gate.load(Ordering::Relaxed) as u64 {
            return Ok(SourceReply::Pending);
        }
        self.source.poll(r, w, c)
    }
    fn invalidate(&mut self) {
        self.calls.lock().unwrap().push("invalidate");
        self.source.invalidate();
    }
    fn abort(&mut self) {
        self.calls.lock().unwrap().push("abort");
        self.source.abort();
    }
    fn close(&mut self) {
        self.calls.lock().unwrap().push("close");
        self.source.close();
    }
}

pub(crate) type Calls = Arc<Mutex<Vec<&'static str>>>;

pub(crate) fn player(
    raw: &[u8],
    live: bool,
    limits: ProgressiveLimits,
) -> (Player<Observed>, Budget, Arc<AtomicUsize>, Calls) {
    let b = pool();
    let gate = Arc::new(AtomicUsize::new(usize::MAX));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let source = Observed {
        source: ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 1).unwrap(),
        gate: gate.clone(),
        calls: calls.clone(),
    };
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: if live { None } else { Some(raw.len() as u64) },
            random_access: !live,
        },
        64,
    )
    .unwrap();
    let mut open = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    run_open(&mut open, 1).unwrap();
    let opened = open.finish().unwrap();
    let mut index = Mp4IndexJob::new(&opened.movie, PlanLimits::default()).unwrap();
    while !index.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let plan = index.finish().unwrap();
    let engine = if live {
        Engine::mp4_progressive(opened.input, plan, 1 << 20, limits).unwrap()
    } else {
        Engine::mp4(opened.input, plan, 1 << 20).unwrap()
    };
    (
        Player::new(
            engine,
            opened.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap(),
        b,
        gate,
        calls,
    )
}
