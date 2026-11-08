//! The libass source the ASS parsing reference compiles in place of the
//! library's `ass_parse.o`: the pinned `ass_parse.c` with every conversion
//! from `double` to `int` that C leaves undefined out of range wrapped in
//! `ASSR_TO_INT`, which `harness/reference_ass.h` defines to note such a
//! value and convert it as the compiler does. Each edit must match exactly as
//! often as listed, so a different libass source fails instead of losing a
//! check.

use std::path::{Path, PathBuf};

type R<T> = Result<T, String>;

/// The pinned source, relative to the root.
const SOURCE: &str = ".research/libass/libass-0.17.5/libass/ass_parse.c";

/// Where the edited copy is written, relative to the root.
pub const DIRECTORY: &str = ".research/libass/observed";

/// The copy's file name, which `harness/reference_ass.h` includes.
const FILE: &str = "ass_parse_observed.c";

/// Each conversion as written in libass 0.17.5, how often it occurs, and the
/// expression converted.
const CONVERSIONS: [(&str, usize, &str); 8] = [
    // `\clip` and `\iclip` with four coordinates, animated by `\t`.
    (
        "state->clip_x0 * (1 - pwr) + x0 * pwr;",
        2,
        "state->clip_x0 * (1 - pwr) + x0 * pwr",
    ),
    (
        "state->clip_x1 * (1 - pwr) + x1 * pwr;",
        2,
        "state->clip_x1 * (1 - pwr) + x1 * pwr",
    ),
    (
        "state->clip_y0 * (1 - pwr) + y0 * pwr;",
        2,
        "state->clip_y0 * (1 - pwr) + y0 * pwr",
    ),
    (
        "state->clip_y1 * (1 - pwr) + y1 * pwr;",
        2,
        "state->clip_y1 * (1 - pwr) + y1 * pwr",
    ),
    // `interpolate_alpha`, for `\fad` and `\fade`.
    ("a = a1 * (1 - cf) + a2 * cf;", 1, "a1 * (1 - cf) + a2 * cf"),
    ("a = a2 * (1 - cf) + a3 * cf;", 1, "a2 * (1 - cf) + a3 * cf"),
    // The delay of a `Banner` and a `Scroll` effect.
    (
        "delay = ((int) FFMAX(delay / scale_x, 1)) * scale_x;",
        1,
        "FFMAX(delay / scale_x, 1)",
    ),
    (
        "delay = ((int) FFMAX(delay / scale_y, 1)) * scale_y;",
        1,
        "FFMAX(delay / scale_y, 1)",
    ),
];

/// The edited source for `source`, the text of the pinned `ass_parse.c`.
fn edit(source: &str) -> R<String> {
    let mut text = source.to_owned();
    for (written, count, converted) in CONVERSIONS {
        let found = text.matches(written).count();
        if found != count {
            return Err(format!(
                "{SOURCE}: `{written}` occurs {found} times, not {count}"
            ));
        }
        let observed = written
            .replacen(
                &format!("((int) {converted})"),
                &format!("ASSR_TO_INT({converted})"),
                1,
            )
            .replacen(
                &format!("{converted};"),
                &format!("ASSR_TO_INT({converted});"),
                1,
            );
        text = text.replace(written, &observed);
    }
    Ok(text)
}

/// Writes the edited copy and returns its directory, for the reference's
/// include path.
pub fn observed_sources(root: &Path) -> R<PathBuf> {
    let path = root.join(SOURCE);
    let source = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let directory = root.join(DIRECTORY);
    std::fs::create_dir_all(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
    let target = directory.join(FILE);
    let text = edit(&source)?;
    if std::fs::read_to_string(&target).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(&target, text).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::edit;

    #[test]
    fn every_conversion_is_wrapped_once_where_written() {
        let source = "x = 1;\n\
            state->clip_x0 = state->clip_x0 * (1 - pwr) + x0 * pwr;\n\
            state->clip_x0 = state->clip_x0 * (1 - pwr) + x0 * pwr;\n\
            state->clip_x1 * (1 - pwr) + x1 * pwr;\nstate->clip_x1 * (1 - pwr) + x1 * pwr;\n\
            state->clip_y0 * (1 - pwr) + y0 * pwr;\nstate->clip_y0 * (1 - pwr) + y0 * pwr;\n\
            state->clip_y1 * (1 - pwr) + y1 * pwr;\nstate->clip_y1 * (1 - pwr) + y1 * pwr;\n\
            a = a1 * (1 - cf) + a2 * cf;\na = a2 * (1 - cf) + a3 * cf;\n\
            delay = ((int) FFMAX(delay / scale_x, 1)) * scale_x;\n\
            delay = ((int) FFMAX(delay / scale_y, 1)) * scale_y;\n";
        let edited = edit(source).unwrap();
        assert!(
            edited.contains("state->clip_x0 = ASSR_TO_INT(state->clip_x0 * (1 - pwr) + x0 * pwr);")
        );
        assert!(edited.contains("a = ASSR_TO_INT(a2 * (1 - cf) + a3 * cf);"));
        assert!(edited.contains("delay = ASSR_TO_INT(FFMAX(delay / scale_y, 1)) * scale_y;"));
        assert_eq!(edited.matches("ASSR_TO_INT").count(), 12);
        assert!(edit("x = 1;").is_err());
    }
}
