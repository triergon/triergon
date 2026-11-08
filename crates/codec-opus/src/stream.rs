//! One Opus stream (RFC 6716 section 4.5): SILK-only, hybrid and CELT-only frames, and
//! the redundant CELT frames and cross-fades of mode transitions.

use crate::DecodeError;
use crate::celt::{self, CeltDecoder, FrameLayout, HYBRID_START_BAND};
use crate::packet::{Mode, Packet, Toc};
use crate::range::{Position, RangeDecoder};
use crate::silk::{MAX_PACKET_PULSES, SilkConfig, SilkDecoder, SilkLayer};
use triergon_codec::{Budget, BudgetVec, StorageError};

/// Samples per channel of the longest packet (120 ms) plus what the SILK resampler may
/// release with it.
pub(crate) const MAX_PACKET: usize = 5760 + 32;
/// Samples of the longest frame (60 ms).
const MAX_FRAME: usize = 2880;
/// A redundant CELT frame lasts 5 ms and is cross-faded over its second half.
const REDUNDANT: usize = 240;
const FADE: usize = 120;
/// Most samples the SILK resampler holds back.
const MAX_HELD: usize = 32;
/// Most frames with a SILK layer in one packet: 120 ms of 10 ms frames.
const MAX_SILK_FRAMES: usize = 12;

/// Buffers of one packet, shared by all streams of a decoder.
#[derive(Debug)]
pub(crate) struct Workspace {
    pub(crate) celt: celt::Workspace,
    /// The decoded packet of the current stream.
    pub(crate) pcm: [BudgetVec<f32>; 2],
    silk: [BudgetVec<f32>; 2],
    redundant: [[f32; REDUNDANT]; 2],
}

impl Workspace {
    /// The buffers and the CELT transforms, charging `budget` for what they allocate.
    pub(crate) fn new(budget: &Budget) -> Result<Workspace, StorageError> {
        let buffer = |len| BudgetVec::try_filled(budget, 0.0, len);
        Ok(Workspace {
            celt: celt::Workspace::new(budget)?,
            pcm: [buffer(MAX_PACKET)?, buffer(MAX_PACKET)?],
            silk: [buffer(MAX_FRAME)?, buffer(MAX_FRAME)?],
            redundant: [[0.0; REDUNDANT]; 2],
        })
    }
}

/// What [`StreamDecoder::check`] reads of a frame with a SILK layer, for
/// [`StreamDecoder::decode`] to continue from: the SILK symbols, the redundant frame
/// and where the CELT layer starts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StagedFrame {
    silk: SilkLayer,
    redundancy: Option<Redundancy>,
    celt: Position,
}

/// The frames [`StreamDecoder::check`] reads of one packet in every stream of a
/// decoder, until [`StreamDecoder::decode`] synthesizes them. Every stream has room for
/// a packet of 120 ms.
#[derive(Debug)]
pub(crate) struct Staging {
    frames: BudgetVec<StagedFrame>,
    pulses: BudgetVec<i32>,
}

impl Staging {
    pub(crate) fn new(streams: usize, budget: &Budget) -> Result<Staging, StorageError> {
        let room = |per_stream: usize| {
            streams
                .checked_mul(per_stream)
                .ok_or(StorageError::Overflow)
        };
        Ok(Staging {
            frames: BudgetVec::try_filled(budget, StagedFrame::default(), room(MAX_SILK_FRAMES)?)?,
            pulses: BudgetVec::try_filled(budget, 0, room(MAX_PACKET_PULSES)?)?,
        })
    }

    /// The room of stream `stream`.
    pub(crate) fn stream(&self, stream: usize) -> (&[StagedFrame], &[i32]) {
        (
            &self.frames[stream * MAX_SILK_FRAMES..][..MAX_SILK_FRAMES],
            &self.pulses[stream * MAX_PACKET_PULSES..][..MAX_PACKET_PULSES],
        )
    }

    /// As [`Staging::stream`], to fill.
    pub(crate) fn stream_mut(&mut self, stream: usize) -> (&mut [StagedFrame], &mut [i32]) {
        (
            &mut self.frames[stream * MAX_SILK_FRAMES..][..MAX_SILK_FRAMES],
            &mut self.pulses[stream * MAX_PACKET_PULSES..][..MAX_PACKET_PULSES],
        )
    }
}

/// A redundant CELT frame in a SILK or hybrid frame (RFC 6716 section 4.5.1).
#[derive(Clone, Copy, Debug)]
struct Redundancy {
    /// The frame starts a CELT-to-SILK transition (else it ends a SILK-to-CELT one).
    celt_to_silk: bool,
    /// Where it starts in the frame, which it ends.
    start: usize,
}

/// Decoder state of one elementary Opus stream of one or two channels.
#[derive(Debug)]
pub(crate) struct StreamDecoder {
    channels: usize,
    silk: SilkDecoder,
    celt: CeltDecoder,
    /// Mode of the last decoded frame.
    previous: Option<Mode>,
    /// The last frame ended in a redundant CELT frame that the next CELT frame continues.
    continues_redundancy: bool,
    final_range: u32,
    /// Internal rate of the last SILK frame, while SILK runs.
    silk_rate: Option<u32>,
    /// SILK output samples the resampler holds back, released when SILK stops or
    /// changes rate.
    held: usize,
    /// The end of a fade into a redundant frame that falls on held-back samples.
    pending_fade: PendingFade,
}

/// The part of a SILK-to-CELT fade that applies to held-back samples: fade positions
/// `from..FADE` with the redundant frame's samples there.
#[derive(Clone, Copy, Debug, Default)]
struct PendingFade {
    from: usize,
    redundant: [[f32; MAX_HELD]; 2],
    len: usize,
}

impl StreamDecoder {
    pub(crate) fn new(channels: usize) -> StreamDecoder {
        StreamDecoder {
            channels,
            silk: SilkDecoder::new(),
            celt: CeltDecoder::new(channels),
            previous: None,
            continues_redundancy: false,
            final_range: 0,
            silk_rate: None,
            held: 0,
            pending_fade: PendingFade::default(),
        }
    }

    /// The range coder state after the last frame, XORed with that of its redundant
    /// frame: the check value of the reference test vectors.
    pub(crate) fn final_range(&self) -> u32 {
        self.final_range
    }

    pub(crate) fn reset(&mut self) {
        self.silk.reset();
        self.celt.reset();
        self.previous = None;
        self.continues_redundancy = false;
        self.final_range = 0;
        self.silk_rate = None;
        self.held = 0;
        self.pending_fade = PendingFade::default();
    }

    /// Refuse `packet` if this stream cannot decode it, and otherwise return the samples
    /// per channel [`StreamDecoder::decode`] writes for it. Each frame's SILK layer and
    /// redundancy signalling (whose size a hybrid frame codes and may not fit) are read
    /// into `stage` for `decode`. Reads no state that decoding changes, so a decoder can
    /// check every stream of a packet before it decodes one.
    pub(crate) fn check(
        &self,
        packet: &Packet<'_>,
        (staged, pulses): (&mut [StagedFrame], &mut [i32]),
    ) -> Result<usize, DecodeError> {
        let toc = packet.toc;
        if let Some(silk) = SilkConfig::of(toc) {
            debug_assert!(packet.frames().len() * silk.pulses_len() <= pulses.len());
            let frames = packet.frames().iter().zip(staged);
            for ((&data, staged), pulses) in frames.zip(pulses.chunks_exact_mut(silk.pulses_len()))
            {
                let mut rc = RangeDecoder::new(data);
                let layer = SilkLayer::parse(&mut rc, silk, pulses);
                *staged = StagedFrame {
                    silk: layer,
                    redundancy: parse_redundancy(&mut rc, toc.mode, data)?,
                    celt: rc.position(),
                };
            }
        }
        Ok(self.output_len(packet))
    }

    /// The samples per channel decoding `packet` writes: its duration, plus those the
    /// SILK resampler releases when SILK stops or changes rate, less those it holds back
    /// when SILK starts. All frames of a packet share the mode and rate, so only the
    /// first can stop or start SILK.
    fn output_len(&self, packet: &Packet<'_>) -> usize {
        let silk = SilkConfig::of(packet.toc);
        let duration = packet.frames().len() * packet.toc.duration.samples();
        if silk.map(SilkConfig::rate) == self.silk_rate {
            duration
        } else {
            duration + self.held - silk.map_or(0, SilkConfig::start_latency)
        }
    }

    /// The samples [`StreamDecoder::flush`] releases.
    pub(crate) fn pending(&self) -> usize {
        self.held
    }

    /// Decode every frame of a `packet` that [`StreamDecoder::check`] accepted, with
    /// what it staged, into `work.pcm[channel][..]` and return the samples per channel
    /// written.
    pub(crate) fn decode(
        &mut self,
        packet: &Packet<'_>,
        (staged, pulses): (&[StagedFrame], &[i32]),
        work: &mut Workspace,
    ) -> usize {
        let toc = packet.toc;
        let mut at = 0;
        match SilkConfig::of(toc) {
            Some(silk) => {
                let frames = packet.frames().iter().zip(staged);
                for ((&data, staged), pulses) in frames.zip(pulses.chunks_exact(silk.pulses_len()))
                {
                    at += self.decode_frame(toc, data, Some((silk, staged, pulses)), at, work);
                }
            }
            None => {
                for &data in packet.frames() {
                    at += self.decode_frame(toc, data, None, at, work);
                }
            }
        }
        at
    }

    /// Release the samples the SILK resampler still holds into `work.pcm[channel][..]`,
    /// as at the end of the stream; returns the samples per channel written.
    pub(crate) fn flush(&mut self, work: &mut Workspace) -> usize {
        self.silk_rate = None;
        self.flush_silk(work, 0)
    }

    fn flush_silk(&mut self, work: &mut Workspace, at: usize) -> usize {
        let [left, right] = &mut work.pcm;
        let mut out = [&mut left[at..], &mut right[at..]];
        let released = self.silk.flush(&mut out[..self.channels]);
        self.held = 0;
        // Finish a fade into a redundant frame that reached into the held samples.
        let fade = core::mem::take(&mut self.pending_fade);
        for (pcm, redundant) in work.pcm.iter_mut().zip(&fade.redundant).take(self.channels) {
            let pcm = &mut pcm[at..at + released.min(fade.len)];
            for (i, (p, &r)) in pcm.iter_mut().zip(redundant).enumerate() {
                *p = celt::fade(fade.from + i, *p, r);
            }
        }
        released
    }

    /// Decode one frame into `work.pcm[..][at..]` (`opus_decode_frame`) and return the
    /// samples per channel written. A frame with a SILK layer comes with what `check`
    /// staged of it and its pulses.
    fn decode_frame(
        &mut self,
        toc: Toc,
        data: &[u8],
        silk: Option<(SilkConfig, &StagedFrame, &[i32])>,
        start: usize,
        work: &mut Workspace,
    ) -> usize {
        let mode = toc.mode;
        let (mut rc, redundancy) = match silk {
            Some((_, staged, _)) => (RangeDecoder::resume(data, staged.celt), staged.redundancy),
            None => (RangeDecoder::new(data), None),
        };

        // The SILK resampler releases what it holds back when SILK stops or changes rate.
        let silk_rate = silk.map(|(config, ..)| config.rate());
        let mut at = start;
        if self.silk_rate.is_some() && self.silk_rate != silk_rate {
            at += self.flush_silk(work, at);
        }
        self.silk_rate = silk_rate;

        let len = match silk {
            Some((config, staged, pulses)) => self.decode_silk(config, &staged.silk, pulses, work),
            None => toc.duration.samples(),
        };
        let mut redundant_range = 0;
        if let Some(Redundancy {
            celt_to_silk: true,
            start,
        }) = redundancy
        {
            // The CELT-to-SILK frame continues the CELT signal before the SILK one starts.
            redundant_range = self.decode_redundant(&data[start..], toc, work);
        }
        self.decode_celt(&mut rc, toc, at, work);
        if mode != Mode::Celt {
            for (pcm, silk) in work.pcm.iter_mut().zip(&work.silk).take(self.channels) {
                for (p, &s) in pcm[at..at + len].iter_mut().zip(&silk[..len]) {
                    *p += s;
                }
            }
        }
        match redundancy {
            Some(Redundancy {
                celt_to_silk: false,
                start,
            }) => {
                // The SILK-to-CELT frame starts the CELT signal the next frame continues.
                self.celt.reset();
                redundant_range = self.decode_redundant(&data[start..], toc, work);
                self.fade_out_into_redundant(work, at + len);
            }
            Some(Redundancy {
                celt_to_silk: true, ..
            }) => fade_in_from_redundant(work, at, self.channels),
            None => {}
        }

        self.final_range = if data.len() <= 1 {
            0
        } else {
            rc.range() ^ redundant_range
        };
        self.previous = Some(mode);
        self.continues_redundancy = redundancy.is_some_and(|r| !r.celt_to_silk);
        at + len - start
    }

    /// Decode the SILK layer into `work.silk`, resampled to 48 kHz by the SILK layer, and
    /// return the samples written: the first narrowband frame after a start comes out
    /// short.
    fn decode_silk(
        &mut self,
        silk: SilkConfig,
        layer: &SilkLayer,
        pulses: &[i32],
        work: &mut Workspace,
    ) -> usize {
        if self.previous == Some(Mode::Celt) {
            self.silk.reset();
        }
        let n = silk.duration_48k();
        let [left, right] = &mut work.silk;
        let mut out = [&mut left[..n], &mut right[..n]];
        let len = self
            .silk
            .decode(silk, layer, pulses, &mut out[..self.channels]);
        self.held += n - len;
        len
    }

    /// The CELT layer of a CELT-only or hybrid frame into `work.pcm[..][at..]`, or silence
    /// for a SILK-only frame. FFmpeg adds no CELT fade-out after a hybrid frame (the
    /// reference decodes a silent CELT frame into the first 2.5 ms of SILK).
    fn decode_celt(
        &mut self,
        rc: &mut RangeDecoder<'_>,
        toc: Toc,
        at: usize,
        work: &mut Workspace,
    ) {
        let mode = toc.mode;
        let n = toc.duration.samples();
        let [left, right] = &mut work.pcm;
        let (left, right) = (&mut left[at..at + n], &mut right[at..at + n]);
        let Some(lm) = toc.duration.celt_lm().filter(|_| mode != Mode::Silk) else {
            left.fill(0.0);
            right.fill(0.0);
            return;
        };
        // A new mode starts the CELT layer afresh unless a redundant frame led into it.
        if self.previous.is_some_and(|previous| previous != mode) && !self.continues_redundancy {
            self.celt.reset();
        }
        let layout = FrameLayout {
            lm,
            start: if mode == Mode::Hybrid {
                HYBRID_START_BAND
            } else {
                0
            },
            end: toc.bandwidth.celt_end_band(),
            coded_channels: toc.channels(),
        };
        self.celt.decode(rc, layout, &mut work.celt, [left, right]);
    }

    /// Fade the frame ending at `end` into the redundant frame over its last 2.5 ms, of
    /// which the SILK resampler may still hold the last samples: their part of the fade
    /// is applied when they are released.
    fn fade_out_into_redundant(&mut self, work: &mut Workspace, end: usize) {
        let held = self.held.min(MAX_HELD);
        let visible = FADE - held;
        for (pcm, redundant) in work.pcm.iter_mut().zip(&work.redundant).take(self.channels) {
            let pcm = &mut pcm[end - visible..end];
            for (i, (p, &r)) in pcm.iter_mut().zip(&redundant[FADE..]).enumerate() {
                *p = celt::fade(i, *p, r);
            }
        }
        let mut pending = PendingFade {
            from: visible,
            len: held,
            ..PendingFade::default()
        };
        for (tail, redundant) in pending.redundant.iter_mut().zip(&work.redundant) {
            tail[..held].copy_from_slice(&redundant[FADE + visible..]);
        }
        self.pending_fade = pending;
    }

    /// Decode a 5 ms redundant CELT frame into `work.redundant` and return its final
    /// range.
    fn decode_redundant(&mut self, data: &[u8], toc: Toc, work: &mut Workspace) -> u32 {
        let layout = FrameLayout {
            lm: 1,
            start: 0,
            end: toc.bandwidth.celt_end_band(),
            coded_channels: toc.channels(),
        };
        let mut rc = RangeDecoder::new(data);
        let [left, right] = &mut work.redundant;
        self.celt.decode(
            &mut rc,
            layout,
            &mut work.celt,
            [&mut left[..], &mut right[..]],
        );
        rc.range()
    }
}

/// The redundant CELT frame signalled after the SILK data of a SILK-only or hybrid
/// frame (RFC 6716 section 4.5.1), whose bytes end the frame; `rc` then excludes them.
/// Only a hybrid frame codes the size, which may not fit: a SILK-only frame's redundant
/// frame takes the bytes after the SILK data.
fn parse_redundancy(
    rc: &mut RangeDecoder<'_>,
    mode: Mode,
    data: &[u8],
) -> Result<Option<Redundancy>, DecodeError> {
    let hybrid_bits = if mode == Mode::Hybrid { 20 } else { 0 };
    if mode == Mode::Celt || rc.tell() + 17 + hybrid_bits > 8 * data.len() as i32 {
        return Ok(None);
    }
    if mode == Mode::Hybrid && !rc.bit_logp(12) {
        return Ok(None);
    }
    let celt_to_silk = rc.bit_logp(1);
    let bytes = if mode == Mode::Hybrid {
        rc.uint(256) as usize + 2
    } else {
        data.len() - ((rc.tell() as usize + 7) >> 3)
    };
    let kept = data
        .len()
        .checked_sub(bytes)
        .filter(|&kept| kept as i32 * 8 >= rc.tell())
        .ok_or(DecodeError::Invalid("redundant frame size"))?;
    rc.shrink(bytes);
    Ok(Some(Redundancy {
        celt_to_silk,
        start: kept,
    }))
}

/// Start the frame at `at` with the redundant frame, then fade from it into the frame.
fn fade_in_from_redundant(work: &mut Workspace, at: usize, channels: usize) {
    for (pcm, redundant) in work.pcm.iter_mut().zip(&work.redundant).take(channels) {
        let pcm = &mut pcm[at..at + REDUNDANT];
        pcm[..FADE].copy_from_slice(&redundant[..FADE]);
        for (i, (p, &r)) in pcm[FADE..].iter_mut().zip(&redundant[FADE..]).enumerate() {
            *p = celt::fade(i, r, *p);
        }
    }
}
