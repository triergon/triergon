//! Inter-channel decorrelation (RFC 9639 section 4.2): left/side, side/right and
//! mid/side frames, whose side channel has one bit more than the frame's sample size.
use crate::output::Output;
use crate::sample::Sample;
use triergon_framing::FlacChannelAssignment;

/// The decorrelated channel pair of a stereo frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stereo {
    /// Left, then left minus right.
    LeftSide,
    /// Left minus right, then right.
    SideRight,
    /// The average rounded down, then left minus right.
    MidSide,
}

impl Stereo {
    /// The pair `assignment` codes; `None` for independent channels.
    pub(crate) fn of(assignment: FlacChannelAssignment) -> Option<Stereo> {
        match assignment {
            FlacChannelAssignment::Independent(_) => None,
            FlacChannelAssignment::LeftSide => Some(Stereo::LeftSide),
            FlacChannelAssignment::SideRight => Some(Stereo::SideRight),
            FlacChannelAssignment::MidSide => Some(Stereo::MidSide),
        }
    }

    /// The side channel is coded first.
    pub(crate) fn side_first(self) -> bool {
        self == Stereo::SideRight
    }

    /// Writes left and right from the plain channel (left, right or mid) and the side.
    /// The arithmetic runs in the side's width; it is exact for every valid stream.
    pub(crate) fn emit<S: Sample>(self, plain: &[i32], side: &[S], out: &mut impl Output) {
        let pairs = || plain.iter().zip(side).map(|(&p, &s)| (S::from(p), s));
        match self {
            Stereo::LeftSide => {
                out.put(0, plain.iter().copied());
                out.put(
                    1,
                    pairs().map(|(left, side)| left.wrapping_sub(side).to_i32()),
                );
            }
            Stereo::SideRight => {
                out.put(
                    0,
                    pairs().map(|(right, side)| right.wrapping_add(side).to_i32()),
                );
                out.put(1, plain.iter().copied());
            }
            Stereo::MidSide => {
                // mid * 2 + (side & 1) is left + right.
                let sum = |mid: S, side: S| mid.wrapping_shl(1) | (side & S::from(1));
                out.put(
                    0,
                    pairs().map(|(mid, side)| (sum(mid, side).wrapping_add(side) >> 1).to_i32()),
                );
                out.put(
                    1,
                    pairs().map(|(mid, side)| (sum(mid, side).wrapping_sub(side) >> 1).to_i32()),
                );
            }
        }
    }
}
