//! Cursors and answers the cursor tests share.
use triergon_core::{Budget, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceId};

/// The source every test cursor starts on.
pub(crate) const SOURCE: SourceId = SourceId(7);

/// A cursor on [`SOURCE`] with a 64-byte window limit.
pub(crate) fn cursor(pool: &Budget, length: Option<u64>, random_access: bool) -> InputCursor {
    cursor_with_window(pool, length, random_access, 64)
}

pub(crate) fn cursor_with_window(
    pool: &Budget,
    length: Option<u64>,
    random_access: bool,
    maximum_window: usize,
) -> InputCursor {
    InputCursor::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length,
            random_access,
        },
        maximum_window,
    )
    .unwrap()
}

/// `bytes` as the answer to the cursor's current request.
pub(crate) fn window(input: &InputCursor, bytes: Bytes) -> InputWindow {
    let request = input.request();
    InputWindow::new(request.source, request.token, request.offset, bytes).unwrap()
}

/// [`window`] over a copy of `bytes` charged to the cursor's pool.
pub(crate) fn copied_window(input: &InputCursor, bytes: &[u8]) -> InputWindow {
    window(
        input,
        Bytes::copy_from_slice(input.budget(), bytes).unwrap(),
    )
}

/// Answers the current request with `bytes` held inline.
pub(crate) fn provide_inline(input: &mut InputCursor, bytes: &[u8]) {
    let request = input.request();
    input
        .provide_inline(request.source, request.token, request.offset, bytes)
        .unwrap();
}

/// Answers the current request with `bytes`, given away.
pub(crate) fn provide_owned(input: &mut InputCursor, bytes: Bytes) {
    let answer = window(input, bytes);
    input.provide_owned(answer).unwrap();
}
