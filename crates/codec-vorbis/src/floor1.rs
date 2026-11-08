//! Floor type 1 (Vorbis I section 7): a piecewise linear curve on a dB scale.

use triergon_bitstream::LsbReader;

use crate::bits::BitWindow;
use crate::codebook::Codebook;
use crate::tables::INVERSE_DB;
use crate::{DecodeError, ilog};

/// X positions a floor 1 may list, the two implicit ends included.
pub(crate) const MAX_POINTS: usize = 65;

/// Y values of one decoded floor 1, in list order.
pub(crate) type Points = [i32; MAX_POINTS];

#[derive(Clone, Copy, Debug, Default)]
struct Class {
    /// Y values one partition of this class codes.
    dimensions: usize,
    /// log2 of the number of subclass books.
    subclass_bits: u32,
    master: Option<u8>,
    books: [Option<u8>; 8],
}

#[derive(Debug)]
pub(crate) struct Floor1 {
    /// Class of each partition, in order.
    partitions: Vec<u8>,
    classes: [Class; 16],
    multiplier: i32,
    /// Number of X positions.
    count: usize,
    x: [i32; MAX_POINTS],
    /// List positions in ascending X order.
    order: [u8; MAX_POINTS],
    /// low_neighbor and high_neighbor (section 9.2.4, 9.2.5) of each position from 2.
    neighbors: [(u8, u8); MAX_POINTS],
}

impl Floor1 {
    /// Reads a floor 1 configuration (section 7.2.2).
    pub(crate) fn read(
        r: &mut LsbReader<'_>,
        codebooks: &[Codebook],
    ) -> Result<Floor1, DecodeError> {
        let book = |number: u32| -> Result<u8, DecodeError> {
            if (number as usize) < codebooks.len() {
                Ok(number as u8)
            } else {
                Err(DecodeError::Invalid("floor 1 codebook number"))
            }
        };
        // Capped at 31 partitions by the field width.
        let partitions: Vec<u8> = (0..r.read(5)).map(|_| r.read(4) as u8).collect();
        let mut classes = [Class::default(); 16];
        let class_count = partitions
            .iter()
            .map(|&class| usize::from(class) + 1)
            .max()
            .unwrap_or(0);
        for class in &mut classes[..class_count] {
            class.dimensions = r.read(3) as usize + 1;
            class.subclass_bits = r.read(2);
            if class.subclass_bits != 0 {
                class.master = Some(book(r.read(8))?);
            }
            for slot in &mut class.books[..1 << class.subclass_bits] {
                *slot = match r.read(8) {
                    0 => None,
                    number => Some(book(number - 1)?),
                };
            }
        }
        let multiplier = r.read(2) as i32 + 1;
        let range_bits = r.read(4);
        let mut x = [0; MAX_POINTS];
        x[1] = 1 << range_bits;
        let mut count = 2;
        for &class in &partitions {
            for _ in 0..classes[usize::from(class)].dimensions {
                let slot = x
                    .get_mut(count)
                    .ok_or(DecodeError::Invalid("floor 1 point count"))?;
                *slot = r.read(range_bits) as i32;
                count += 1;
            }
        }
        if r.overrun() {
            return Err(DecodeError::Invalid("truncated floor 1"));
        }
        let mut order = [0u8; MAX_POINTS];
        for (i, slot) in order[..count].iter_mut().enumerate() {
            *slot = i as u8;
        }
        order[..count].sort_unstable_by_key(|&i| x[usize::from(i)]);
        if order[..count]
            .windows(2)
            .any(|pair| x[usize::from(pair[0])] == x[usize::from(pair[1])])
        {
            return Err(DecodeError::Invalid("floor 1 repeats an X position"));
        }
        let mut neighbors = [(0, 0); MAX_POINTS];
        for i in 2..count {
            let (mut low, mut high) = (0, 1);
            for j in 0..i {
                if x[j] < x[i] && x[j] > x[low] {
                    low = j;
                }
                if x[j] > x[i] && x[j] < x[high] {
                    high = j;
                }
            }
            neighbors[i] = (low as u8, high as u8);
        }
        Ok(Floor1 {
            partitions,
            classes,
            multiplier,
            count,
            x,
            order,
            neighbors,
        })
    }

    fn range(&self) -> i32 {
        [256, 128, 86, 64][self.multiplier as usize - 1]
    }

    /// Reads the Y values of one channel (section 7.2.3). False if the channel is unused
    /// in this packet.
    pub(crate) fn decode(
        &self,
        r: &mut LsbReader<'_>,
        codebooks: &[Codebook],
        y: &mut Points,
    ) -> bool {
        r.bit() && self.read_points(r, codebooks, y).is_some()
    }

    fn read_points(
        &self,
        r: &mut LsbReader<'_>,
        codebooks: &[Codebook],
        y: &mut Points,
    ) -> Option<()> {
        let r = &mut BitWindow::new(r);
        let bits = ilog(self.range() as u32 - 1);
        y[0] = r.read(bits) as i32;
        y[1] = r.read(bits) as i32;
        let mut points = y[2..].iter_mut();
        for &class in &self.partitions {
            let class = &self.classes[usize::from(class)];
            let mut choice = match class.master {
                Some(master) => codebooks[usize::from(master)].entry(r)?,
                None => 0,
            };
            let mask = (1 << class.subclass_bits) - 1;
            for point in points.by_ref().take(class.dimensions) {
                *point = match class.books[(choice & mask) as usize] {
                    Some(book) => codebooks[usize::from(book)].entry(r)? as i32,
                    None => 0,
                };
                choice >>= class.subclass_bits;
            }
        }
        Some(())
    }

    /// Multiplies `spectrum` by the curve of the decoded `y` (section 7.2.4).
    pub(crate) fn apply(&self, y: &Points, spectrum: &mut [f32]) {
        let range = self.range();
        let count = self.count;
        // Step 1: unwrap the coded differences against the line prediction.
        let mut level = [0i32; MAX_POINTS];
        let mut drawn = [false; MAX_POINTS];
        level[..2].copy_from_slice(&y[..2]);
        drawn[..2].fill(true);
        for i in 2..count {
            let (low, high) = self.neighbors[i];
            let (low, high) = (usize::from(low), usize::from(high));
            let predicted = render_point(
                self.x[low],
                level[low],
                self.x[high],
                level[high],
                self.x[i],
            );
            let value = y[i];
            let high_room = range - predicted;
            let low_room = predicted;
            let room = 2 * high_room.min(low_room);
            level[i] = if value == 0 {
                predicted
            } else {
                drawn[low] = true;
                drawn[high] = true;
                drawn[i] = true;
                if value >= room {
                    if high_room > low_room {
                        value - low_room + predicted
                    } else {
                        predicted - value + high_room - 1
                    }
                } else if value & 1 == 1 {
                    predicted - (value + 1) / 2
                } else {
                    predicted + value / 2
                }
            };
        }
        // Step 2: draw the lines between the points that carry information, in X order.
        let scale = |level: i32| level.clamp(0, range - 1) * self.multiplier;
        let mut from = (0, scale(level[usize::from(self.order[0])]));
        for &i in &self.order[1..count] {
            let i = usize::from(i);
            if drawn[i] {
                let to = (self.x[i], scale(level[i]));
                render_line(from, to, spectrum);
                from = to;
            }
        }
        let n = spectrum.len() as i32;
        if from.0 < n {
            render_line(from, (n, from.1), spectrum);
        }
    }
}

/// render_point (section 9.2.6).
fn render_point(x0: i32, y0: i32, x1: i32, y1: i32, x: i32) -> i32 {
    let dy = y1 - y0;
    let offset = dy.abs() * (x - x0) / (x1 - x0);
    if dy < 0 { y0 - offset } else { y0 + offset }
}

/// render_line (section 9.2.7) from `(x0, y0)` up to but excluding `x1`, applied as a
/// gain: each element of `spectrum` it covers is multiplied by the floor value there.
/// The levels must lie in `0..=255`, as every level between them then does.
///
/// The line's level `k` samples on is `y0 + k * base + sign * floor(k * ady / adx)` with
/// `ady < adx`. A shallow line (`base` 0, long runs of one level) is applied run by run;
/// any other in interleaved lanes.
pub(crate) fn render_line((x0, y0): (i32, i32), (x1, y1): (i32, i32), spectrum: &mut [f32]) {
    let end = (x1.max(0) as usize).min(spectrum.len());
    let start = x0.max(0) as usize;
    if start >= end {
        return;
    }
    let dy = y1 - y0;
    let line = Line {
        y0,
        adx: x1 - x0,
        base: dy / (x1 - x0),
        sign: if dy < 0 { -1 } else { 1 },
        ady: dy.abs() % (x1 - x0),
    };
    let samples = &mut spectrum[start..end];
    if line.base == 0 && line.ady * MIN_RUN <= line.adx {
        line.apply_runs(samples);
    } else {
        line.apply_lanes(samples);
    }
}

/// The average run of one level from which a line is applied run by run.
const MIN_RUN: i32 = 8;

/// Interleaved lanes [`Line::apply_lanes`] steps at once.
const LANES: usize = 4;

/// The integer line of render_line, as an offset `k` from its start.
#[derive(Clone, Copy, Debug)]
struct Line {
    y0: i32,
    adx: i32,
    base: i32,
    sign: i32,
    /// `|dy| mod adx`: the error term's step.
    ady: i32,
}

fn gain(y: i32) -> f32 {
    INVERSE_DB[(y & 0xff) as usize]
}

impl Line {
    /// A level changes by `sign` at the `j`-th carry, at `k = ceil(j * adx / ady)`; the
    /// distance between carries is `adx / ady` or one more, stepped by its own error term.
    fn apply_runs(self, samples: &mut [f32]) {
        if self.ady == 0 {
            let gain = gain(self.y0);
            for value in samples {
                *value *= gain;
            }
            return;
        }
        let (step, fraction) = (self.adx / self.ady, self.adx % self.ady);
        let (mut y, mut rest, mut slack) = (self.y0, samples, 0);
        while !rest.is_empty() {
            // `slack` is `ady * ceil(j * fraction / ady) - j * fraction` for the next carry.
            let mut run = step;
            slack -= fraction;
            if slack < 0 {
                slack += self.ady;
                run += 1;
            }
            let (head, tail) = rest.split_at_mut((run as usize).min(rest.len()));
            let gain = gain(y);
            for value in head {
                *value *= gain;
            }
            rest = tail;
            y += self.sign;
        }
    }

    /// Every [`LANES`]-th sample follows a line of its own: the lanes step independently,
    /// which keeps the error-term recurrence off a single dependency chain.
    fn apply_lanes(self, samples: &mut [f32]) {
        let Line {
            y0,
            adx,
            base,
            sign,
            ady,
        } = self;
        // The specification's recurrence for the first lane of samples.
        let mut err = [0; LANES];
        let mut level = [0; LANES];
        let (mut e, mut y) = (0, y0);
        for (err, level) in err.iter_mut().zip(&mut level) {
            e += ady;
            y += base;
            if e >= adx {
                e -= adx;
                y += sign;
            }
            (*err, *level) = (e, y);
        }
        let lanes = LANES as i32;
        // `ady < adx`, so a lane carries at most `LANES` times per step.
        let carries = (1..=lanes).filter(|&k| lanes * ady >= k * adx).count() as i32;
        let jump = lanes * base + sign * carries;
        let remainder = lanes * ady - carries * adx;
        let (first, rest) = samples.split_at_mut(1);
        first[0] *= gain(y0);
        let mut chunks = rest.chunks_exact_mut(LANES);
        for chunk in &mut chunks {
            for (value, &y) in chunk.iter_mut().zip(&level) {
                *value *= gain(y);
            }
            for (e, y) in err.iter_mut().zip(&mut level) {
                let carry = i32::from(*e + remainder >= adx);
                *e += remainder - carry * adx;
                *y += jump + carry * sign;
            }
        }
        for (value, &y) in chunks.into_remainder().iter_mut().zip(&level) {
            *value *= gain(y);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The floor values `render_line` draws, read back as table indices.
    fn drawn(from: (i32, i32), to: (i32, i32), len: usize) -> Vec<usize> {
        let mut spectrum = vec![1.0f32; len];
        render_line(from, to, &mut spectrum);
        spectrum
            .iter()
            .map(|&v| {
                INVERSE_DB
                    .iter()
                    .position(|&g| g.to_bits() == v.to_bits())
                    .unwrap_or(usize::MAX)
            })
            .collect()
    }

    #[test]
    fn render_line_steps_like_bresenham() {
        assert_eq!(drawn((0, 0), (8, 4), 8), [0, 0, 1, 1, 2, 2, 3, 3]);
        assert_eq!(drawn((0, 10), (4, 0), 4), [10, 8, 5, 3]);
        assert_eq!(drawn((0, 3), (5, 3), 5), [3; 5]);
        // Steep: base 3, remainder 1 every fourth step.
        assert_eq!(drawn((0, 0), (4, 13), 4), [0, 3, 6, 9]);
        // Clipped at the vector end.
        assert_eq!(drawn((2, 0), (10, 8), 4)[2..], [0, 1]);
    }

    #[test]
    fn lanes_follow_the_specification_recurrence() {
        // Section 9.2.7 one sample at a time.
        let specified = |(x0, y0): (i32, i32), (x1, y1): (i32, i32)| {
            let (dy, adx) = (y1 - y0, x1 - x0);
            let base = dy / adx;
            let step = if dy < 0 { base - 1 } else { base + 1 };
            let ady = dy.abs() - base.abs() * adx;
            let (mut y, mut err) = (y0, 0);
            let mut out = vec![y0 as usize];
            for _ in x0 + 1..x1 {
                err += ady;
                if err >= adx {
                    err -= adx;
                    y += step;
                } else {
                    y += base;
                }
                out.push(y as usize);
            }
            out
        };
        for adx in 1..40 {
            for y1 in (0..256).step_by(7) {
                for y0 in [0, 3, 100, 255] {
                    let line = ((5, y0), (5 + adx, y1));
                    let len = 5 + adx as usize;
                    assert_eq!(
                        drawn(line.0, line.1, len)[5..],
                        specified(line.0, line.1)[..]
                    );
                }
            }
        }
    }

    #[test]
    fn render_point_truncates_towards_the_start() {
        assert_eq!(render_point(0, 0, 10, 5, 3), 1);
        assert_eq!(render_point(0, 5, 10, 0, 3), 4);
        assert_eq!(render_point(16, 20, 32, 120, 24), 70);
    }
}
