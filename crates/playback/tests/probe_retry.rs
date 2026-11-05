use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error as CoreError, InputRequest, InputWindow, SourceCapabilities, SourceId,
    WorkBudget,
};
use triergon_playback::{
    Error, Format, Source, SourceReply,
    io::{Probe, ProbeProgress},
};
const ID: SourceId = SourceId(9121);
const RAW: &[u8] = include_bytes!("fixtures/mp4/r-front-stco.mov");
#[derive(Default)]
struct Calls {
    polls: usize,
    aborts: usize,
    closes: usize,
}
struct Once {
    bytes: Option<Bytes>,
    calls: Arc<Mutex<Calls>>,
}
impl Source for Once {
    fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if c.load(Ordering::Relaxed) {
            return Err(CoreError::Cancelled);
        }
        if r.source != ID {
            return Err(CoreError::SourceMismatch);
        }
        if !w.try_charge(1) {
            return Ok(SourceReply::Pending);
        }
        self.calls.lock().unwrap().polls += 1;
        let bytes = self.bytes.take().ok_or(CoreError::Invalid(
            "one-shot producer cannot replay consumed input",
        ))?;
        if r.offset != 0 || bytes.len() > r.maximum {
            return Err(CoreError::Invalid("original range request changed"));
        }
        Ok(SourceReply::Data(InputWindow::new(
            ID,
            r.token.clone(),
            0,
            bytes,
        )?))
    }
    fn invalidate(&mut self) {}
    fn abort(&mut self) {
        self.calls.lock().unwrap().aborts += 1;
    }
    fn close(&mut self) {
        self.calls.lock().unwrap().closes += 1;
    }
}
fn setup() -> (Probe<Once>, Budget, Arc<Mutex<Calls>>) {
    let b = Budget::new(4 << 20).unwrap();
    let calls = Arc::new(Mutex::new(Calls::default()));
    let source = Once {
        bytes: Some(Bytes::copy_from_slice(&b, RAW).unwrap()),
        calls: calls.clone(),
    };
    let p = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(RAW.len() as u64),
            random_access: false,
        },
        RAW.len(),
        source,
    )
    .unwrap();
    (p, b, calls)
}
fn select(p: &mut Probe<Once>) {
    for _ in 0..2048 {
        if let ProbeProgress::Selected(f) = p
            .advance(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            assert_eq!(f, Format::Mp4);
            return;
        }
    }
    panic!("bounded source probe did not select real MP4 fixture");
}
#[test]
fn probe_cache_pressure_retains_one_shot_data_without_repoll_or_close() {
    let (mut p, b, calls) = setup();
    let c = AtomicBool::new(false);
    assert!(matches!(
        p.advance(&mut WorkBudget::new(1), &c).unwrap(),
        ProbeProgress::NeedInput(_)
    ));
    let held = b.reserve(b.available()).unwrap();
    assert!(matches!(
        p.advance(&mut WorkBudget::new(1), &c),
        Err(Error::Core(CoreError::Backpressure { .. }))
    ));
    {
        let x = calls.lock().unwrap();
        assert_eq!(x.polls, 1);
        assert_eq!(
            x.aborts, 0,
            "transient allocation pressure must not abort the external producer"
        );
        assert_eq!(x.closes, 0);
    }
    drop(held);
    let used = b.used();
    assert!(matches!(
        p.advance(&mut WorkBudget::new(0), &c).unwrap(),
        ProbeProgress::Yield
    ));
    assert_eq!(b.used(), used);
    assert_eq!(calls.lock().unwrap().polls, 1);
    select(&mut p);
    let mut selected = p.finish().unwrap();
    let reply = selected
        .source
        .poll(&selected.input.request(), &mut WorkBudget::new(1), &c)
        .unwrap();
    let SourceReply::Data(window) = &reply else {
        panic!("actual cached source owner lost");
    };
    assert_eq!(window.bytes().as_ref(), RAW);
    assert_eq!(window.source, ID);
    assert_eq!(window.offset, 0);
    assert!(window.bytes().belongs_to(&b));
    assert_eq!(calls.lock().unwrap().polls, 1);
    selected.input.provide(window).unwrap();
    drop(reply);
    drop(selected);
    {
        let x = calls.lock().unwrap();
        assert_eq!(x.aborts, 1);
        assert_eq!(x.closes, 1);
    }
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn replay_foreign_source_requests_refuse_without_aliasing_cache_or_polling_upstream() {
    let (mut p, b, calls) = setup();
    select(&mut p);
    let mut selected = p.finish().unwrap();
    let c = AtomicBool::new(false);
    for offset in [0, RAW.len() as u64 + 1] {
        let mut request = selected.input.request();
        request.source = SourceId(ID.0 + 1);
        request.offset = offset;
        let before = b.used();
        let mut work = WorkBudget::new(1);
        assert!(
            matches!(
                selected.source.poll(&request, &mut work, &c),
                Err(Error::Core(CoreError::SourceMismatch))
            ),
            "foreign public Source request must not receive original cached bytes"
        );
        assert_eq!(work.remaining(), 1);
        assert_eq!(b.used(), before);
        assert_eq!(calls.lock().unwrap().polls, 1);
    }
    let reply = selected
        .source
        .poll(&selected.input.request(), &mut WorkBudget::new(1), &c)
        .unwrap();
    let SourceReply::Data(window) = &reply else {
        panic!("correct source cache was damaged");
    };
    assert_eq!(window.source, ID);
    assert_eq!(window.bytes().as_ref(), RAW);
    drop(reply);
    drop(selected);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
