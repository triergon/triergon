//! The receiver's run-length decoding of an assembled object.
//!
//! The receiver fills the `width * height` plane through one cursor, row
//! boundaries included: an end-of-line code does not move the cursor to the
//! next row, it only counts a line, and decoding stops after `height` of them.
//! A run that would pass the end of the plane is dropped whole, and bytes read
//! past the end of the data are zero. The plane is accepted only if it is
//! filled exactly.

/// Decodes `encoded` into `plane` (`rows` lines), returning whether every
/// pixel was written.
///
/// A code is a nonzero colour (one pixel) or `0x00` and a flags byte: bits 0-5
/// are the run length, bit 6 extends it with the next byte, bit 7 adds an
/// explicit colour byte (otherwise colour 0). A zero length ends a line.
pub(crate) fn decode(encoded: &[u8], rows: usize, plane: &mut [u8]) -> bool {
    let byte = |at: usize| encoded.get(at).copied().unwrap_or(0);
    let mut at = 0;
    let mut filled = 0;
    let mut lines = 0;
    while lines < rows
        && let Some(&first) = encoded.get(at)
    {
        if first != 0 {
            if let Some(pixel) = plane.get_mut(filled) {
                *pixel = first;
                filled += 1;
            }
            at += 1;
            continue;
        }
        let flags = byte(at + 1);
        at += 2;
        let mut run = usize::from(flags & 0x3f);
        if flags & 0x40 != 0 {
            run = run << 8 | usize::from(byte(at));
            at += 1;
        }
        let colour = if flags & 0x80 != 0 {
            at += 1;
            byte(at - 1)
        } else {
            0
        };
        if run == 0 {
            lines += 1;
        } else {
            fill(plane, &mut filled, colour, run);
        }
    }
    filled == plane.len()
}

/// Short runs are written as one fixed block: any bytes past the run are
/// rewritten by the runs that follow, since a plane is only accepted once
/// every pixel after `filled` has been written in order.
const BLOCK: usize = 64;

/// `run` pixels of `colour` at `filled`, or nothing if they would pass the end.
#[inline]
fn fill(plane: &mut [u8], filled: &mut usize, colour: u8, run: usize) {
    if run <= BLOCK && *filled + BLOCK <= plane.len() {
        plane[*filled..*filled + BLOCK].copy_from_slice(&[colour; BLOCK]);
    } else if let Some(target) = plane.get_mut(*filled..*filled + run) {
        target.fill(colour);
    } else {
        return;
    }
    *filled += run;
}

#[cfg(test)]
mod tests {
    use super::decode;

    /// A 4x2 plane, or 1x1 for `area` 1.
    fn plane(encoded: &[u8], area: usize) -> Option<Vec<u8>> {
        let mut out = vec![0xee; area];
        let rows = if area == 1 { 1 } else { area / 4 };
        decode(encoded, rows, &mut out).then_some(out)
    }

    #[test]
    fn rows_share_one_cursor_and_overlong_runs_are_dropped() {
        // 4x2: six single pixels spill into the second row; the last two
        // single pixels no longer fit and are dropped.
        assert_eq!(
            plane(&[1, 1, 1, 1, 1, 1, 0, 0, 2, 2, 2, 2, 0, 0], 8),
            Some(vec![1, 1, 1, 1, 1, 1, 2, 2])
        );
        // A six-pixel run at cursor 4 does not fit and is skipped whole.
        assert_eq!(
            plane(&[1, 1, 1, 1, 0, 0x86, 2, 3, 3, 3, 3, 0, 0], 8),
            Some(vec![1, 1, 1, 1, 3, 3, 3, 3])
        );
    }

    #[test]
    fn zero_length_runs_end_lines_and_missing_bytes_read_as_zero() {
        assert_eq!(plane(&[0, 0x80, 5, 0, 0x88, 1], 8), Some(vec![1; 8]));
        assert_eq!(plane(&[0, 0x40, 0, 0, 0x88, 1], 8), Some(vec![1; 8]));
        // A colour run cut before its colour byte paints colour 0.
        assert_eq!(
            plane(&[0, 0x87, 1, 0, 0x81], 8),
            Some(vec![1, 1, 1, 1, 1, 1, 1, 0])
        );
        assert_eq!(plane(&[0, 0x87, 1, 0, 0x80], 8), None);
        assert_eq!(plane(&[0, 0x87, 1, 0, 0xc0], 8), None);
    }

    #[test]
    fn decoding_stops_after_one_line_end_per_row() {
        // 4x2: a five-pixel first line shifts the second; its line end is
        // the last one read, so the third line never fills the plane.
        assert_eq!(plane(&[1, 1, 1, 1, 1, 0, 0, 2, 2, 0, 0, 3, 0, 0], 8), None);
        assert_eq!(
            plane(&[1, 1, 1, 1, 0, 0, 2, 2, 2, 2, 0, 0, 3, 3, 0, 0], 8),
            Some(vec![1, 1, 1, 1, 2, 2, 2, 2])
        );
    }

    #[test]
    fn the_plane_must_be_filled_exactly() {
        assert_eq!(plane(&[1, 0, 0, 2, 0, 0], 8), None);
        assert_eq!(plane(&[0, 0x88, 1, 1, 1], 8), Some(vec![1; 8]));
        assert_eq!(plane(&[], 1), None);
        assert_eq!(plane(&[0, 0x81], 1), Some(vec![0]));
    }
}
