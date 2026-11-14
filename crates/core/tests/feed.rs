use std::cell::Cell;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::AtomicBool;
use triergon_core::blocking::Feed;
use triergon_core::{
    Budget, Error, InputCursor, OwnedInputBytes, SourceCapabilities, SourceId, Step, WorkBudget,
};

fn cursor(pool: &Budget, length: u64, window: usize) -> InputCursor {
    InputCursor::new(
        pool,
        SourceId(3),
        SourceCapabilities {
            length: Some(length),
            random_access: true,
        },
        window,
    )
    .unwrap()
}

/// Reads the whole source through `feed`, one window per request.
fn drain<S: Read + Seek>(input: &mut InputCursor, feed: &mut Feed<S>) -> Vec<u8> {
    let cancel = AtomicBool::new(false);
    let mut out = Vec::new();
    loop {
        let mut byte = [0u8; 256];
        match input
            .read_into(&mut byte, &mut WorkBudget::new(1 << 20), &cancel)
            .unwrap()
        {
            Step::Ready(count) => out.extend_from_slice(&byte[..count]),
            Step::NeedInput(request) => feed.supply(input, request).unwrap(),
            Step::End(_) => return out,
            other => panic!("unexpected step {other:?}"),
        }
    }
}

/// Counts the seeks it is asked for.
struct Counting<'a> {
    inner: Cursor<Vec<u8>>,
    seeks: &'a Cell<usize>,
}
impl Read for Counting<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}
impl Seek for Counting<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.seeks.set(self.seeks.get() + 1);
        self.inner.seek(to)
    }
}

#[test]
fn every_window_size_reads_the_whole_source() {
    let bytes: Vec<u8> = (0..20_000u32).map(|i| (i * 7 + 3) as u8).collect();
    let pool = Budget::new(1 << 20).unwrap();
    // One byte, inline, a small owned window and the largest.
    for window in [1, InputCursor::INLINE_CAPACITY, 4096, 65536] {
        let mut input = cursor(&pool, bytes.len() as u64, window);
        let mut feed = Feed::new(Cursor::new(bytes.clone()), &pool).unwrap();
        assert_eq!(drain(&mut input, &mut feed), bytes, "window {window}");
    }
}

#[test]
fn the_source_is_seeked_only_when_a_request_does_not_continue() {
    let bytes: Vec<u8> = (0..64u8).collect();
    let pool = Budget::new(1 << 20).unwrap();
    let mut input = cursor(&pool, 64, 1);
    let seeks = Cell::new(0);
    let source = Counting {
        inner: Cursor::new(bytes.clone()),
        seeks: &seeks,
    };
    let mut feed = Feed::new(source, &pool).unwrap();
    let rewinds = seeks.get();
    assert_eq!(drain(&mut input, &mut feed), bytes);
    assert_eq!(seeks.get(), rewinds);

    let request = input.request_range(40).unwrap();
    feed.supply(&mut input, request).unwrap();
    assert_eq!(seeks.get(), rewinds + 1);
    assert_eq!(input.buffered(), &[40]);
}

#[test]
fn a_failed_read_fails_the_cursor() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("transport"))
        }
    }
    impl Seek for Broken {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }
    let pool = Budget::new(1 << 20).unwrap();
    let mut input = cursor(&pool, 8, 4);
    let mut feed = Feed::new(Broken, &pool).unwrap();
    let request = input.request();
    assert!(matches!(
        feed.supply(&mut input, request),
        Err(Error::Io(io::ErrorKind::Other))
    ));
    assert!(matches!(
        input.read_into(&mut [0], &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::Transport)
    ));
}

#[test]
fn a_short_source_ends_the_entity() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut input = cursor(&pool, 3, 2);
    let mut feed = Feed::new(Cursor::new(vec![1, 2, 3]), &pool).unwrap();
    assert_eq!(drain(&mut input, &mut feed), [1, 2, 3]);
}

#[test]
fn owned_feed_shares_large_windows_and_resumes_with_inline_bytes() {
    let pool = Budget::new(4096).unwrap();
    let bytes: Vec<_> = (0..37_u8).collect();
    let storage = OwnedInputBytes::try_from_vec(&pool, bytes.clone()).unwrap();
    let mut input = cursor(&pool, bytes.len() as u64, 32);
    let mut feed = Feed::from_owned(&storage, &pool).unwrap();
    let request = input.request();
    let before = pool.used();
    feed.supply(&mut input, request).unwrap();
    let retained = input.retained_window().unwrap();
    assert!(retained.shares_storage_with(storage.as_bytes()));
    assert_eq!(pool.used(), before);
    assert_eq!(feed.position(), 32);
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        input
            .read_discard(32, &mut WorkBudget::new(32), &cancel)
            .unwrap(),
        Step::Ready(32)
    ));
    assert_eq!(drain(&mut input, &mut feed), &bytes[32..]);
    assert_eq!(retained.as_ref(), &bytes[..32]);
    assert_eq!(feed.into_inner().position(), 37);
    drop(retained);
    assert_eq!(storage.try_into_vec().unwrap(), bytes);
}

#[test]
fn a_mutated_source_is_repositioned_before_the_next_read() {
    let pool = Budget::new(4096).unwrap();
    let mut input = cursor(&pool, 8, 4);
    let mut feed = Feed::new(Cursor::new((0..8_u8).collect::<Vec<_>>()), &pool).unwrap();
    let request = input.request();
    feed.supply(&mut input, request).unwrap();
    assert!(matches!(
        input
            .read_discard(4, &mut WorkBudget::new(4), &AtomicBool::new(false))
            .unwrap(),
        Step::Ready(4)
    ));
    feed.source_mut().set_position(0);
    let request = input.request();
    feed.supply(&mut input, request).unwrap();
    assert_eq!(input.buffered(), [4, 5, 6, 7]);
}
