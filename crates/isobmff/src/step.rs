//! The values the crate's polls return: [`Next`] for walks that hand out each item,
//! [`BuildStatus`] for builders that are finished separately, and [`CursorStatus`] for
//! cursors whose items are taken separately.

/// One step of a walk that hands out each item directly.
#[derive(Debug, PartialEq, Eq)]
pub enum Next<T> {
    /// An item consumed from the walk.
    Item(T),
    /// The work budget is spent: poll again with a new one.
    Yield,
    /// The walk has no more items; later polls also return `End`.
    End,
}

/// One poll of a builder that is finished separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildStatus {
    /// The work budget is spent: poll again with a new one.
    Yield,
    /// The result is complete: call the builder's `finish`.
    Ready,
}

/// One poll of a cursor whose items are taken separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStatus {
    /// The work budget is spent: poll again with a new one.
    Yield,
    /// An item is ready to take. A cursor may also report readiness after setup.
    Ready,
    /// The cursor has exhausted its items under the current source scope.
    End,
}
