#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`.

mod codebook;
mod floor0;
mod floor1;
mod mapping;
mod residue;
mod setup;
mod spectra;
mod tables;
mod window;

use triergon_bitstream::LsbReader;
use triergon_codec::layout::{native_order, vorbis_order};
use triergon_codec::mdct::Imdct;
use triergon_codec::{
    AudioDecoder, AudioFrame, Budget, BudgetVec, ChannelLayout, DecodeFailure, DownmixLevels,
    Reservation, StorageError,
};
use triergon_framing::{VorbisHeaders, VorbisIdentification};

use floor0::Coefficients;
use floor1::Points;
use mapping::{Mapping, Mode, decouple};
use residue::Member;
use setup::{Floor, Setup};
use spectra::Spectra;
use window::{Block, Slopes, fade_out, fold, overlap_add};

/// Bounds of one decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Heap bytes the decoder may hold, output frame included: the setup header's
    /// codebook, floor and residue tables, the per-channel block buffers and the frame
    /// it fills. The default, 32 MiB, holds 93 channels of 8192-sample blocks (about
    /// 7 MiB) next to setup tables many times those of any encoder.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 32 << 20,
        }
    }
}

/// Why a decoder refused a configuration or an access unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A feature this decoder does not implement.
    Unsupported(&'static str),
    /// Malformed data.
    Invalid(&'static str),
    /// The storage budget refused an allocation.
    Budget(StorageError),
}

impl From<StorageError> for DecodeError {
    fn from(error: StorageError) -> Self {
        DecodeError::Budget(error)
    }
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::Unsupported(what) => write!(f, "unsupported: {what}"),
            DecodeError::Invalid(what) => write!(f, "invalid data: {what}"),
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// ilog (Vorbis I section 9.2.1): the position of the highest set bit.
pub(crate) fn ilog(x: u32) -> u32 {
    u32::BITS - x.leading_zeros()
}

/// Appends `value` to `list`, charging room for it unless reserved.
pub(crate) fn push<T>(list: &mut BudgetVec<T>, value: T) -> Result<(), StorageError> {
    list.try_push(value).map_err(|refused| refused.error)
}

/// The floor one channel decoded in the current packet.
#[derive(Clone, Copy, Debug)]
enum Curve {
    /// No energy: the channel's output is zero.
    Unused,
    /// Floor 0 with this amplitude and the channel's `coefficients`.
    Lsp(u64),
    /// Floor 1 with the channel's `points`.
    Linear,
}

#[derive(Clone, Debug)]
struct ChannelFloor {
    curve: Curve,
    points: Points,
    coefficients: Coefficients,
}

/// A Vorbis I decoder.
///
/// Each audio packet yields the samples from the centre of the previous block to the
/// centre of its own, `previous / 4 + current / 4` per channel; the first packet after
/// construction or [`AudioDecoder::reset`] only primes the overlap and yields none.
/// Planes follow FFmpeg's native channel order.
#[derive(Debug)]
pub struct Decoder {
    sample_rate: u32,
    layout: ChannelLayout,
    /// Output plane of each coded channel.
    planes: [u8; ChannelLayout::CAPACITY],
    blocksizes: [usize; 2],
    setup: Setup,
    imdct: [Imdct; 2],
    slopes: Slopes,
    /// The spectrum of each channel in the current block.
    spectra: Spectra,
    /// The last output quarter of each channel's previous inverse MDCT, which with its
    /// reverse makes the right half the next block laps.
    overlap: Spectra,
    floors: BudgetVec<ChannelFloor>,
    /// The DCT-IV of one channel's spectrum.
    dct: BudgetVec<f32>,
    scratch: residue::Scratch,
    /// The block size of the previous packet; `None` until a packet primed the overlap.
    previous: Option<Block>,
    /// Charge of the output frame the last packet sized.
    output: Reservation,
}

/// Scale of the inverse MDCT that gives output at full scale 1.0.
const IMDCT_SCALE: f64 = 1.0;

impl Decoder {
    /// A decoder for `configuration`, the identification, comment and setup headers
    /// Xiph-laced (Matroska `CodecPrivate`), with a pool of the default limit.
    pub fn new(configuration: &[u8]) -> Result<Decoder, DecodeError> {
        Decoder::with_limits(configuration, Limits::default())
    }

    /// As [`Decoder::new`], with a pool of `limits.live_storage_bytes`.
    pub fn with_limits(configuration: &[u8], limits: Limits) -> Result<Decoder, DecodeError> {
        Decoder::new_in(configuration, &Budget::new(limits.live_storage_bytes)?)
    }

    /// As [`Decoder::new`], charging the caller's pool.
    pub fn new_in(configuration: &[u8], budget: &Budget) -> Result<Decoder, DecodeError> {
        let headers = VorbisHeaders::from_laced(configuration)
            .map_err(|_| DecodeError::Invalid("Vorbis configuration"))?;
        Decoder::from_headers_in(
            headers.identification,
            headers.comment,
            headers.setup,
            budget,
        )
    }

    /// A decoder for the identification, comment and setup header packets as an Ogg
    /// stream carries them, charging the caller's pool.
    pub fn from_headers_in(
        identification: &[u8],
        comment: &[u8],
        setup: &[u8],
        budget: &Budget,
    ) -> Result<Decoder, DecodeError> {
        let id = VorbisIdentification::parse(identification).map_err(|error| match error {
            StorageError::Unsupported(what) => DecodeError::Unsupported(what),
            _ => DecodeError::Invalid("identification header"),
        })?;
        if !comment.starts_with(b"\x03vorbis") {
            return Err(DecodeError::Invalid("comment header"));
        }
        let channels = usize::from(id.channels);
        if channels > ChannelLayout::CAPACITY {
            return Err(DecodeError::Unsupported(
                "more channels than an output layout holds",
            ));
        }
        let blocksizes = id.blocksize.map(usize::from);
        let (layout, planes) = output_layout(channels);
        let setup = Setup::parse(setup, channels, blocksizes, budget)?;

        let half = blocksizes[1] / 2;
        let imdct = [
            Imdct::new(blocksizes[0], IMDCT_SCALE, budget)?,
            Imdct::new(blocksizes[1], IMDCT_SCALE, budget)?,
        ];
        let floor = ChannelFloor {
            curve: Curve::Unused,
            points: [0; floor1::MAX_POINTS],
            coefficients: [0.0; floor0::MAX_ORDER],
        };
        let interleaves = setup.residues.iter().any(residue::Residue::interleaves);
        let classes = setup
            .residues
            .iter()
            .map(|residue| residue.class_slots(channels, half))
            .max()
            .unwrap_or(0);
        Ok(Decoder {
            sample_rate: id.sample_rate,
            layout,
            planes,
            blocksizes,
            imdct,
            slopes: Slopes::new(blocksizes, budget)?,
            spectra: Spectra::new(channels, half, budget)?,
            overlap: Spectra::new(channels, half, budget)?,
            floors: BudgetVec::try_filled(budget, floor, channels)?,
            dct: BudgetVec::try_filled(budget, 0.0, half)?,
            scratch: residue::Scratch {
                interleaved: BudgetVec::try_filled(
                    budget,
                    0.0,
                    if interleaves { channels * half } else { 0 },
                )?,
                classes: BudgetVec::try_filled(budget, 0, classes)?,
            },
            setup,
            previous: None,
            output: Reservation::empty(budget),
        })
    }

    /// Output sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Output channel layout.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    fn decode_packet(&mut self, packet: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        let r = &mut LsbReader::new(packet);
        let mode = self.read_mode(r)?;
        let block = mode.block;
        let half = self.blocksizes[block as usize] / 2;
        let Decoder {
            setup,
            spectra,
            floors,
            scratch,
            ..
        } = self;
        let mapping = &setup.mappings[usize::from(mode.mapping)];
        decode_floors(setup, mapping, r, floors)?;
        // Spectral lines past `coded` are zero in every channel until the transform.
        let coded = decode_residues(setup, mapping, r, half, floors, spectra, scratch);
        for step in mapping.coupling.iter().rev() {
            let (magnitude, angle) =
                spectra.pair_mut(usize::from(step.magnitude), usize::from(step.angle));
            decouple(&mut magnitude[..coded], &mut angle[..coded]);
        }
        apply_floors(setup, mapping, block, floors, spectra, coded);
        self.synthesize(block, out)
    }

    /// Packet type, mode and window flags (section 4.3.1). The window shape follows from
    /// the sizes of this and the previous block, so the flags are read and not used.
    fn read_mode(&self, r: &mut LsbReader<'_>) -> Result<Mode, DecodeError> {
        if r.bit() {
            return Err(DecodeError::Invalid("not an audio packet"));
        }
        let modes = &self.setup.modes;
        let number = r.read(ilog(modes.len() as u32 - 1)) as usize;
        let mode = *modes
            .get(number)
            .ok_or(DecodeError::Invalid("mode number"))?;
        if mode.block == Block::Long {
            r.read(2);
        }
        if r.overrun() {
            return Err(DecodeError::Invalid("truncated audio packet"));
        }
        Ok(mode)
    }

    /// Inverse MDCT and overlap-add of every channel into `out` (sections 4.3.7, 4.3.8).
    fn synthesize(&mut self, block: Block, out: &mut AudioFrame) -> Result<(), DecodeError> {
        let half = self.blocksizes[block as usize] / 2;
        let quarter = half / 2;
        let len = self.previous.map_or(0, |previous| {
            self.blocksizes[previous as usize] / 4 + quarter
        });
        out.try_resize(self.layout.channels(), len, &mut self.output)?;
        out.layout = self.layout;
        out.sample_rate = self.sample_rate;
        out.samples_per_channel = len;
        let imdct = &mut self.imdct[block as usize];
        let dct = &mut self.dct[..half];
        for ((spectrum, saved), (floor, &plane)) in self
            .spectra
            .channels_mut()
            .zip(self.overlap.channels_mut())
            .zip(self.floors.iter().zip(&self.planes))
        {
            let lapped = self.previous.map(|previous| {
                let len = self.blocksizes[previous as usize] / 4;
                (&saved[..len], self.slopes.between(previous, block))
            });
            let out = &mut out.planes[usize::from(plane)];
            if matches!(floor.curve, Curve::Unused) {
                if let Some((previous, slope)) = lapped {
                    fade_out(previous, slope, out);
                }
                saved[..quarter].fill(0.0);
            } else {
                imdct.dct4(&spectrum[..half], dct);
                if let Some((previous, slope)) = lapped {
                    overlap_add(previous, dct, slope, out);
                }
                fold(dct, &mut saved[..quarter]);
            }
        }
        self.previous = Some(block);
        Ok(())
    }
}

/// Floor decode of every channel (section 4.3.2).
fn decode_floors(
    setup: &Setup,
    mapping: &Mapping,
    r: &mut LsbReader<'_>,
    floors: &mut [ChannelFloor],
) -> Result<(), DecodeError> {
    for (floor, &submap) in floors.iter_mut().zip(mapping.mux.iter()) {
        let number = mapping.submaps[usize::from(submap)].floor;
        floor.curve = match &setup.floors[usize::from(number)] {
            Floor::Lsp(lsp) => match lsp.decode(r, &setup.codebooks, &mut floor.coefficients)? {
                Some(amplitude) => Curve::Lsp(amplitude),
                None => Curve::Unused,
            },
            Floor::Linear(linear) => {
                if linear.decode(r, &setup.codebooks, &mut floor.points) {
                    Curve::Linear
                } else {
                    Curve::Unused
                }
            }
        };
    }
    Ok(())
}

/// Residue decode by submap (sections 4.3.3, 4.3.4): a channel's residue is coded if
/// its floor is used or it is coupled with a channel whose floor is. Returns how many
/// leading spectral lines of any channel may be nonzero.
fn decode_residues(
    setup: &Setup,
    mapping: &Mapping,
    r: &mut LsbReader<'_>,
    half: usize,
    floors: &[ChannelFloor],
    spectra: &mut Spectra,
    scratch: &mut residue::Scratch,
) -> usize {
    let mut coded = [false; 256];
    for (coded, floor) in coded.iter_mut().zip(floors) {
        *coded = !matches!(floor.curve, Curve::Unused);
    }
    for step in mapping.coupling.iter() {
        let (m, a) = (usize::from(step.magnitude), usize::from(step.angle));
        if coded[m] || coded[a] {
            coded[m] = true;
            coded[a] = true;
        }
    }
    for channel in spectra.channels_mut() {
        channel[..half].fill(0.0);
    }
    let mut coded_lines = 0;
    for (number, submap) in mapping.submaps.iter().enumerate() {
        let mut members = [Member::default(); 256];
        let mut count = 0;
        for (channel, _) in mapping
            .mux
            .iter()
            .enumerate()
            .filter(|&(_, &s)| usize::from(s) == number)
        {
            members[count] = Member {
                channel: channel as u8,
                coded: coded[channel],
            };
            count += 1;
        }
        let lines = setup.residues[usize::from(submap.residue)].decode(
            r,
            &setup.codebooks,
            half,
            &members[..count],
            spectra,
            scratch,
        );
        coded_lines = coded_lines.max(lines);
    }
    coded_lines
}

/// The dot product of each channel's floor curve and residue (section 4.3.6) over the
/// first `coded` lines; the floor leaves the zero lines after them zero.
fn apply_floors(
    setup: &Setup,
    mapping: &Mapping,
    block: Block,
    floors: &[ChannelFloor],
    spectra: &mut Spectra,
    coded: usize,
) {
    for ((spectrum, floor), &submap) in spectra.channels_mut().zip(floors).zip(mapping.mux.iter()) {
        let spectrum = &mut spectrum[..coded];
        let number = mapping.submaps[usize::from(submap)].floor;
        match (floor.curve, &setup.floors[usize::from(number)]) {
            (Curve::Lsp(amplitude), Floor::Lsp(lsp)) => {
                lsp.apply(block, amplitude, &floor.coefficients, spectrum);
            }
            (Curve::Linear, Floor::Linear(linear)) => linear.apply(&floor.points, spectrum),
            _ => spectrum.fill(0.0),
        }
    }
}

/// FFmpeg's layout for `channels` coded channels and the output plane of each: the
/// Vorbis order mapped to native order up to 8 channels; beyond that, coded order
/// without positions.
fn output_layout(channels: usize) -> (ChannelLayout, [u8; ChannelLayout::CAPACITY]) {
    if let Some(mapped) = vorbis_order(channels).and_then(native_order) {
        return mapped;
    }
    let mut planes = [0u8; ChannelLayout::CAPACITY];
    for (i, plane) in planes.iter_mut().enumerate() {
        *plane = i as u8;
    }
    let layout = ChannelLayout::unspecified(channels).unwrap_or_default();
    (layout, planes)
}

impl AudioDecoder for Decoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        self.decode_packet(au, out)
            .map(|()| None)
            .map_err(DecodeFailure::from_error)
    }

    fn reset(&mut self) {
        self.previous = None;
    }
}
