//! The tracks a reader takes from its program's active PMT.
use triergon_core::{Error, Result};
use triergon_mpegts::psi::{ElementaryStream, ProgramMap};

/// The PID of the one stream of `map` that `accepts` takes, among the streams on `pid`
/// when one is given; `None` when no stream qualifies. Fails with `Unsupported` and
/// `ambiguous` when two streams qualify, and with what `accepts` fails with.
pub(crate) fn select_pid(
    map: &ProgramMap,
    pid: Option<u16>,
    accepts: impl Fn(&ElementaryStream) -> Result<bool>,
    ambiguous: &'static str,
) -> Result<Option<u16>> {
    let mut selected = None;
    for stream in map.streams.as_slice() {
        if pid.is_some_and(|pid| stream.pid != pid) || !accepts(stream)? {
            continue;
        }
        if selected.replace(stream.pid).is_some() {
            return Err(Error::Unsupported(ambiguous));
        }
    }
    Ok(selected)
}
