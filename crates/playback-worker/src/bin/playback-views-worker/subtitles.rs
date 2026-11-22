use playback_worker::runner::Result;

pub(super) fn run(_args: &[String]) -> Result<()> {
    Err("usage: playback-views-worker PREFIX{fold,bench,memory,trace} FILE ...; prefixes: text-bridge- subtitle-bridge-".into())
}
