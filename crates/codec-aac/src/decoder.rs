//! raw_data_block() decoding: element parsing, channel mapping, per-channel state and
//! output assembly.

use crate::compat::layout::Policy;
use crate::error::{DecodeError, Feature, Result};
use crate::fft::Dct4;
use crate::filterbank::Filterbank;
use crate::ics::*;
use crate::pce::{Pce, parse_pce};
use crate::tables::SAMPLE_RATES;
use crate::{ltp, pred, sbr};
use triergon_bitstream::CachedBitReader;
use triergon_codec::{AudioFrame, Budget, ChannelLayout, Charged, Reservation, Speaker};

pub(crate) const ID_SCE: u8 = 0;
pub(crate) const ID_CPE: u8 = 1;
pub(crate) const ID_CCE: u8 = 2;
pub(crate) const ID_LFE: u8 = 3;
pub(crate) const ID_DSE: u8 = 4;
pub(crate) const ID_PCE: u8 = 5;
pub(crate) const ID_FIL: u8 = 6;
pub(crate) const ID_END: u8 = 7;

const EXT_SBR_DATA: u32 = 13;
const EXT_SBR_DATA_CRC: u32 = 14;
/// Samples per channel of one raw data block with SBR.
const SBR_FRAME_LEN: usize = 2048;
const PNS_INITIAL_STATE: u32 = 0x1f2e3d4c;

#[derive(Clone, Copy)]
pub(crate) enum FixedLayout {
    Adts,
    Raw(Policy),
}

/// Persistent per-channel state.
#[derive(Debug)]
pub(crate) struct ChannelState {
    pub overlap: [f32; 1024],
    pub prev_shape: u8,
    pub prev_seq: u8,
    pub pred: Option<pred::PredState>,
    pub ltp: Option<ltp::LtpState>,
}

impl ChannelState {
    fn new(budget: &Budget, aot: u8) -> Result<ChannelState> {
        Ok(ChannelState {
            overlap: [0.0; 1024],
            prev_shape: 0,
            prev_seq: 0,
            pred: if aot == AOT_MAIN {
                Some(pred::PredState::new(budget)?)
            } else {
                None
            },
            ltp: if aot == AOT_LTP {
                Some(ltp::LtpState::new(budget)?)
            } else {
                None
            },
        })
    }

    fn reset(&mut self) {
        self.overlap.fill(0.0);
        self.prev_shape = 0;
        self.prev_seq = 0;
        if let Some(p) = &mut self.pred {
            p.reset();
        }
        if let Some(l) = &mut self.ltp {
            l.reset();
        }
    }
}

/// SBR state for an element of `kind` at core sampling-frequency index `sf_index`.
fn new_sbr(budget: &Budget, sf_index: usize, kind: u8) -> Result<Charged<Box<sbr::Sbr>>> {
    let s = sbr::Sbr::new(SAMPLE_RATES[sf_index], kind == ID_CPE, budget)?;
    Ok(Charged::boxed(budget, s)?)
}

/// One output slot: syntactic element + speakers of its channel(s).
#[derive(Clone, Debug)]
pub(crate) struct Slot {
    pub kind: u8,
    pub tag: u8,
    pub speakers: [Option<Speaker>; 2],
}

/// Element instance state (channels + SBR).
#[derive(Debug)]
pub(crate) struct ElementState {
    pub kind: u8,
    pub tag: u8,
    pub ch: [ChannelState; 2],
    pub sbr: Option<Charged<Box<sbr::Sbr>>>,
    /// Time output of the core for this frame (per channel, 1024 samples), and after SBR/PS
    /// up to 2048.
    pub out: [[f32; 2048]; 2],
    /// Set when the element was decoded in the current raw_data_block.
    pub decoded: bool,
    /// The element's channel count (2 for a mono SBR element carrying PS).
    pub nch: usize,
    /// Spectral data of the current frame (filterbank deferred to the END element).
    pub cd: [Charged<Box<ChannelData>>; 2],
    /// element_instance_tag as coded in the bitstream this block (CCE targets refer to it).
    pub bs_tag: u8,
    /// Which channels were decoded this block (an SCE may fill one channel of a CPE slot).
    pub ch_decoded: [bool; 2],
}

impl ElementState {
    fn new(budget: &Budget, aot: u8, kind: u8, tag: u8) -> Result<ElementState> {
        Ok(ElementState {
            kind,
            tag,
            ch: [
                ChannelState::new(budget, aot)?,
                ChannelState::new(budget, aot)?,
            ],
            sbr: None,
            out: [[0.0; 2048]; 2],
            decoded: false,
            nch: if kind == ID_CPE { 2 } else { 1 },
            cd: [
                Charged::boxed(budget, ChannelData::new())?,
                Charged::boxed(budget, ChannelData::new())?,
            ],
            ch_decoded: [false; 2],
            bs_tag: tag,
        })
    }

    /// Return to the state of `new` in place: a layout switch reuses the element of an
    /// identity both layouts have without charging it again.
    fn renew(&mut self) {
        let ElementState {
            kind,
            tag,
            ch,
            sbr,
            out,
            decoded,
            nch,
            cd,
            bs_tag,
            ch_decoded,
        } = self;
        for c in ch {
            c.reset();
        }
        if let Some(s) = sbr {
            s.renew();
        }
        *out = [[0.0; 2048]; 2];
        *decoded = false;
        *nch = if *kind == ID_CPE { 2 } else { 1 };
        for cd in cd {
            let cd: &mut ChannelData = cd;
            *cd = ChannelData::new();
        }
        *bs_tag = *tag;
        *ch_decoded = [false; 2];
    }
}

#[derive(Debug)]
pub(crate) struct Core {
    pub aot: u8,
    pub sf_index: usize,
    pub channel_config: u8,
    pub slots: Vec<Slot>,
    pub layout: ChannelLayout,
    /// plane index for slot i channel c
    pub plane_of: Vec<[usize; 2]>,
    pub elements: Vec<Charged<Box<ElementState>>>,
    pub fb: Filterbank,
    pub random: u32,
    /// SBR has been seen (implicit signalling): output at twice the core rate.
    pub sbr_active: bool,
    /// PS has been seen on a mono element: output stereo.
    pub ps_active: bool,
    pub pce: Option<Pce>,
    /// Keep slot order for planes (FFmpeg unspecified-order layouts).
    pub unordered: bool,
    pub configured: bool,
    /// Coupling channel state (one per CCE tag)
    pub cce: Vec<Charged<Box<crate::cce::CceState>>>,
    /// DCT-IV of every SBR synthesis bank.
    sbr_dct: Dct4,
    /// Pool of the element, SBR and coupling-channel state the stream creates.
    pub budget: Budget,
    /// Charge of the output frame the last access unit sized.
    output_charge: Reservation,
    /// Development statistics of the last block: one line per decoded element (only
    /// collected when `stats_enabled`).
    #[cfg(feature = "inspect")]
    pub stats: String,
    #[cfg(feature = "inspect")]
    pub stats_enabled: bool,
    /// Output samples (per channel) still to be dropped (FFmpeg skip_samples behaviour).
    pub skip_samples: usize,
    pub priming_policy: crate::compat::priming::Policy,
    /// Frames output since the last (re)configuration.
    pub frames_out: u64,
    /// Development aid: bit ranges (relative to the payload) of SBR fill elements of the
    /// last frame.
    #[cfg(feature = "inspect")]
    pub sbr_fil_ranges: Vec<(usize, usize)>,
}

impl Core {
    pub(crate) fn new(budget: &Budget) -> Core {
        Core {
            aot: AOT_LC,
            sf_index: 4,
            channel_config: 0,
            slots: Vec::new(),
            layout: ChannelLayout::default(),
            plane_of: Vec::new(),
            elements: Vec::new(),
            fb: Filterbank::new(),
            random: PNS_INITIAL_STATE,
            sbr_active: false,
            ps_active: false,
            pce: None,
            unordered: false,
            configured: false,
            cce: Vec::new(),
            sbr_dct: sbr::synthesis_dct(),
            budget: budget.clone(),
            output_charge: Reservation::empty(budget),
            #[cfg(feature = "inspect")]
            stats: String::new(),
            #[cfg(feature = "inspect")]
            stats_enabled: false,
            skip_samples: 0,
            priming_policy: crate::compat::priming::Policy::default(),
            #[cfg(feature = "inspect")]
            sbr_fil_ranges: Vec::new(),
            frames_out: 0,
        }
    }

    pub(crate) fn reset_history(&mut self) {
        self.random = PNS_INITIAL_STATE;
        for e in &mut self.elements {
            for c in &mut e.ch {
                c.reset();
            }
            if let Some(s) = &mut e.sbr {
                s.reset();
            }
        }
        for c in &mut self.cce {
            c.reset(self.aot);
        }
    }

    pub(crate) fn output_rate(&self) -> u32 {
        let r = SAMPLE_RATES[self.sf_index];
        if self.sbr_active { r * 2 } else { r }
    }

    pub(crate) fn frame_len(&self) -> usize {
        if self.sbr_active { SBR_FRAME_LEN } else { 1024 }
    }

    /// Select the audio object type, indexed rate and channel configuration.
    pub(crate) fn configure(
        &mut self,
        aot: u8,
        sf_index: u8,
        channel_config: u8,
        layout: FixedLayout,
    ) -> Result<()> {
        match aot {
            AOT_MAIN | AOT_LC | AOT_LTP => {}
            3 => return Err(DecodeError::Unsupported(Feature::SsrGainControl)),
            _ => return Err(DecodeError::Unsupported(Feature::ObjectType(aot))),
        }
        if sf_index > 12 {
            return Err(DecodeError::Unsupported(Feature::SampleRateIndex(sf_index)));
        }
        if self.configured
            && aot == self.aot
            && usize::from(sf_index) == self.sf_index
            && channel_config == self.channel_config
        {
            return Ok(());
        }
        // The new configuration starts from nothing. Until its element state is charged
        // the decoder is unconfigured, so a refusal here is retried by the next header.
        self.elements.clear();
        self.cce.clear();
        self.configured = false;
        let slots = if channel_config == 0 {
            Vec::new()
        } else {
            fixed_config_slots(channel_config, layout)
        };
        let elements = self.build_elements(aot, &slots, false, &[])?;
        self.aot = aot;
        self.sf_index = usize::from(sf_index);
        self.channel_config = channel_config;
        self.sbr_active = false;
        self.ps_active = false;
        self.frames_out = 0;
        self.elements = elements;
        self.configured = true;
        self.unordered = false;
        self.slots = slots;
        if channel_config != 0 {
            self.pce = None;
        }
        self.rebuild_layout();
        Ok(())
    }

    /// Set up the out-of-band program of a raw AAC-LC stream.
    pub(crate) fn with_program(
        budget: &Budget,
        frequency_index: u8,
        program: &Pce,
    ) -> Result<Self> {
        if frequency_index > 12 {
            return Err(DecodeError::Unsupported(Feature::SampleRateIndex(
                frequency_index,
            )));
        }
        if program.object_type != AOT_LC - 1 || program.sf_index != frequency_index {
            return Err(DecodeError::InvalidData("raw AAC program profile or rate"));
        }
        let channels = program
            .front
            .iter()
            .chain(&program.side)
            .chain(&program.back)
            .map(|element| 1 + usize::from(element.is_cpe))
            .sum::<usize>()
            + program.lfe.len();
        if channels == 0 {
            return Err(DecodeError::InvalidData("empty raw AAC program"));
        }
        if channels > 8 {
            return Err(DecodeError::Unsupported(Feature::TooManyChannels));
        }
        let (slots, unordered) = pce_slots(program, false);
        if unordered {
            return Err(DecodeError::Unsupported(Feature::RawProgramLayout));
        }
        for (index, slot) in slots.iter().enumerate() {
            if slots[..index]
                .iter()
                .any(|prior| prior.kind == slot.kind && prior.tag == slot.tag)
            {
                return Err(DecodeError::InvalidData(
                    "duplicate raw AAC program element",
                ));
            }
        }
        let mut core = Self::new(budget);
        core.sf_index = usize::from(frequency_index);
        core.slots = slots;
        core.rebuild_layout();
        if core.layout.mask() == 0 {
            return Err(DecodeError::Unsupported(Feature::RawProgramLayout));
        }
        core.elements = core.build_elements(AOT_LC, &core.slots, false, &[])?;
        core.configured = true;
        Ok(core)
    }

    /// The state of every element `slots` declare (once per element identity, a kind and
    /// a tag) that `existing` lacks, with SBR when `sbr`, built before a configuration
    /// applies so that a refused charge applies nothing.
    fn build_elements(
        &self,
        aot: u8,
        slots: &[Slot],
        sbr: bool,
        existing: &[Charged<Box<ElementState>>],
    ) -> Result<Vec<Charged<Box<ElementState>>>> {
        let mut elements: Vec<Charged<Box<ElementState>>> = Vec::with_capacity(slots.len());
        for s in slots {
            let has = |e: &Charged<Box<ElementState>>| e.kind == s.kind && e.tag == s.tag;
            if elements.iter().any(has) || existing.iter().any(has) {
                continue;
            }
            let mut e = ElementState::new(&self.budget, aot, s.kind, s.tag)?;
            if sbr {
                e.sbr = Some(new_sbr(&self.budget, self.sf_index, s.kind)?);
            }
            elements.push(Charged::boxed(&self.budget, e)?);
        }
        Ok(elements)
    }

    /// Room in `out` for the access unit's remaining `blocks` raw data blocks as
    /// `emit_planes` will fill them in a layout of `channels` planes at `frame_len`, made
    /// before an in-band reconfiguration applies.
    fn make_room(
        &mut self,
        out: &mut AudioFrame,
        channels: usize,
        frame_len: usize,
        blocks: usize,
    ) -> Result<()> {
        // A change of plane count restarts the collection, as in `emit_planes`.
        let base = if out.planes.len() == channels {
            out.planes.first().map_or(0, Vec::len)
        } else {
            0
        };
        let len = base + blocks * frame_len;
        if out.planes.len() >= channels
            && out.planes[..channels].iter().all(|p| p.capacity() >= len)
        {
            return Ok(());
        }
        out.try_resize(channels, len, &mut self.output_charge)?;
        for p in &mut out.planes {
            p.truncate(base);
        }
        Ok(())
    }

    pub(crate) fn rebuild_layout(&mut self) {
        (self.layout, self.plane_of) = self.layout_of(&self.slots, self.unordered);
    }

    /// The layout of `slots` and the plane of each slot's channels.
    fn layout_of(&self, slots: &[Slot], unordered: bool) -> (ChannelLayout, Vec<[usize; 2]>) {
        // Expand slots to planes; PS doubles the single mono slot.
        let mut entries: Vec<(Speaker, usize, usize)> = Vec::new();
        for (i, s) in slots.iter().enumerate() {
            let mut sp = s.speakers;
            if self.ps_active && s.kind == ID_SCE && slots.len() == 1 {
                sp = [Some(Speaker::FrontLeft), Some(Speaker::FrontRight)];
            }
            for (c, x) in sp.iter().enumerate() {
                if let Some(x) = x {
                    entries.push((*x, i, c));
                }
            }
        }
        // stable sort by speaker id: FFmpeg native order
        if !unordered {
            entries.sort_by_key(|e| e.0);
        }
        let mut plane_of = vec![[usize::MAX; 2]; slots.len()];
        for (p, e) in entries.iter().enumerate() {
            plane_of[e.1][e.2] = p;
        }
        // A PCE yields one slot per listed element: at most CAPACITY planes.
        let layout = ChannelLayout::new(entries.iter().map(|e| e.0)).expect("PCE channel bound");
        (layout, plane_of)
    }

    /// Apply an in-band layout of `slots`: its elements and the room it needs in `out` are
    /// charged first, so a refusal leaves the configuration as it was.
    fn switch_layout(
        &mut self,
        slots: Vec<Slot>,
        unordered: bool,
        out: &mut AudioFrame,
        blocks: usize,
    ) -> Result<()> {
        let (layout, plane_of) = self.layout_of(&slots, unordered);
        self.make_room(out, layout.channels(), self.frame_len(), blocks)?;
        // Both layouts together hold at most 48 identities (16 tags of each kind), so the
        // switch never holds more element state than that.
        let mut added = self.build_elements(self.aot, &slots, self.sbr_active, &self.elements)?;
        let mut old = core::mem::take(&mut self.elements);
        for s in &slots {
            let has = |e: &Charged<Box<ElementState>>| e.kind == s.kind && e.tag == s.tag;
            if self.elements.iter().any(has) {
                continue;
            }
            let e = match old.iter().position(has) {
                Some(i) => {
                    let mut e = old.swap_remove(i);
                    e.renew();
                    e
                }
                None => added.remove(added.iter().position(has).expect("built above")),
            };
            self.elements.push(e);
        }
        drop(old);
        self.cce.clear();
        self.slots = slots;
        self.unordered = unordered;
        self.layout = layout;
        self.plane_of = plane_of;
        Ok(())
    }

    fn element_index(&self, kind: u8, tag: u8, occurrence: usize) -> Option<usize> {
        // map to a slot
        let slot = if self.channel_config != 0 {
            // FFmpeg maps fixed configurations by element order, ignoring tags.
            let mut n = 0;
            let mut found = None;
            for (i, s) in self.slots.iter().enumerate() {
                if s.kind == kind {
                    if n == occurrence {
                        found = Some(i);
                        break;
                    }
                    n += 1;
                }
            }
            found
        } else {
            self.slots
                .iter()
                .position(|s| s.kind == kind && s.tag == tag)
        };
        // Every slot's element exists once its configuration applies.
        let s = &self.slots[slot?];
        self.elements
            .iter()
            .position(|e| e.kind == s.kind && e.tag == s.tag)
    }

    /// Decode one raw_data_block(); appends `frame_len()` samples per plane to `planes`.
    /// Decode one raw_data_block() of the `blocks` (this one included) the access unit has
    /// left; `out` gets room for an in-band reconfiguration before it applies.
    pub(crate) fn decode_block<const RAW: bool>(
        &mut self,
        br: &mut CachedBitReader<'_>,
        block_start: usize,
        out: &mut AudioFrame,
        blocks: usize,
    ) -> Result<bool> {
        for e in &mut self.elements {
            e.decoded = false;
            e.ch_decoded = [false; 2];
        }
        for c in &mut self.cce {
            c.decoded = false;
        }
        let cfg = CoreCfg {
            aot: self.aot,
            sf_index: self.sf_index,
        };
        let mut occ = [0usize; 4];
        let fixed_raw = RAW && self.channel_config != 0;
        let mut raw_elements = 0;
        let mut last_elem: Option<usize> = None;
        let mut layout_changed = false;
        loop {
            if br.left() < 3 {
                return Err(DecodeError::InvalidData("missing END element"));
            }
            let id = br.read(3) as u8;
            match id {
                ID_SCE | ID_LFE | ID_CPE => {
                    let tag = br.read(4) as u8;
                    let o = occ[id as usize];
                    let mut ei = if fixed_raw {
                        // Fixed configurations list the audible elements in wire order.
                        let index = raw_elements;
                        raw_elements += 1;
                        self.elements
                            .get(index)
                            .filter(|element| element.kind == id)
                            .map(|_| index)
                    } else if RAW {
                        raw_elements += 1;
                        self.elements.iter().position(|element| {
                            element.kind == id && element.tag == tag && !element.decoded
                        })
                    } else {
                        occ[id as usize] += 1;
                        self.element_index(id, tag, o)
                    };
                    if ei.is_none() && self.channel_config == 2 && id == ID_SCE && o < 2 && !RAW {
                        // FFmpeg maps SCEs of a channel_configuration 2 stream to FL then FR
                        // (probed); the CPE slot's channel o carries them.
                        if let Some(i) = self.element_index(ID_CPE, 0, 0) {
                            self.elements[i].bs_tag = tag;
                            self.decode_sce(br, &cfg, i, o)?;
                            last_elem = Some(i);
                            continue;
                        }
                    }
                    if ei.is_none() && self.channel_config == 1 && id == ID_CPE && o == 0 && !RAW {
                        // FFmpeg switches a channel_configuration 1 stream to stereo when a
                        // CPE appears (probed).
                        self.switch_layout(
                            fixed_config_slots(2, FixedLayout::Adts),
                            self.unordered,
                            out,
                            blocks,
                        )?;
                        self.channel_config = 2;
                        layout_changed = true;
                        ei = self.element_index(id, tag, o);
                    }
                    let ei = ei.ok_or(DecodeError::InvalidData(
                        "element not in channel configuration",
                    ))?;
                    self.elements[ei].bs_tag = tag;
                    if id == ID_CPE {
                        self.decode_cpe(br, &cfg, ei)?;
                    } else {
                        self.decode_sce(br, &cfg, ei, 0)?;
                    }
                    last_elem = Some(ei);
                }
                ID_CCE => {
                    crate::cce::parse_cce::<RAW>(self, br, &cfg)?;
                    last_elem = None;
                }
                ID_DSE => {
                    br.skip(4);
                    let align = br.bit();
                    let mut count = br.read(8) as usize;
                    if count == 255 {
                        count += br.read(8) as usize;
                    }
                    if align {
                        let rel = br.position() - block_start;
                        br.skip((8 - (rel & 7)) & 7);
                    }
                    br.skip(count * 8);
                    last_elem = None;
                }
                ID_PCE => {
                    let p = parse_pce(br, block_start)?;
                    // The out-of-band program stays in effect for raw access units.
                    if !RAW && self.channel_config == 0 && self.pce.as_ref() != Some(&p) {
                        let (slots, unordered) = pce_slots(&p, true);
                        self.switch_layout(slots, unordered, out, blocks)?;
                        self.pce = Some(p);
                        layout_changed = true;
                    }
                    last_elem = None;
                }
                ID_FIL => {
                    #[cfg(feature = "inspect")]
                    let fil_start = br.position() - 3;
                    let mut count = br.read(4) as usize;
                    if count == 15 {
                        count += br.read(8) as usize;
                        count -= 1;
                    }
                    if count > 0 {
                        let end = br.position() + count * 8;
                        if (br.left() as usize) < count * 8 {
                            return Err(DecodeError::InvalidData("fill element overrun"));
                        }
                        let ext = br.read(4);
                        if RAW && matches!(ext, EXT_SBR_DATA | EXT_SBR_DATA_CRC) {
                            return Err(DecodeError::Unsupported(Feature::RawSbr));
                        }
                        if !RAW
                            && ext == 0
                            && count * 8 - 4 >= 13 + 7 * 8
                            && self.priming_policy
                                == crate::compat::priming::Policy::EncoderSignature
                        {
                            // EXT_FILL carrying an encoder signature. FFmpeg skips the first
                            // 1024 output samples when it reads "libfaac <major>.<minor>" here
                            // (faac encoder delay compensation); replicated for output parity.
                            let mut r = *br;
                            r.skip(13);
                            let mut left = count * 8 - 4 - 13;
                            let mut txt = [0u8; 255];
                            let mut n = 0;
                            while left >= 8 && n < 255 {
                                txt[n] = r.read(8) as u8;
                                n += 1;
                                left -= 8;
                            }
                            if is_libfaac_signature(&txt[..n]) {
                                self.skip_samples = 1024;
                            }
                        }
                        if let (EXT_SBR_DATA | EXT_SBR_DATA_CRC, Some(ei)) = (ext, last_elem) {
                            #[cfg(feature = "inspect")]
                            self.sbr_fil_ranges.push((fil_start, end));
                            let saved_end = br.end();
                            br.set_end(end);
                            let crc = ext == EXT_SBR_DATA_CRC;
                            let r = self.decode_sbr_ext(br, ei, crc, count * 8 - 4, out, blocks);
                            br.set_end(saved_end);
                            if r? {
                                layout_changed = true;
                            }
                        }
                        // skip the remainder of the fill element
                        let pos = br.position();
                        if pos < end {
                            br.skip(end - pos);
                        } else if pos > end {
                            return Err(DecodeError::InvalidData("extension payload overrun"));
                        }
                    }
                }
                ID_END => break,
                _ => unreachable!(),
            }
            if br.overrun() {
                return Err(DecodeError::InvalidData("raw_data_block overrun"));
            }
        }
        // byte_alignment after END
        let rel = br.position() - block_start;
        br.skip((8 - (rel & 7)) & 7);
        if RAW && (br.overrun() || br.left() != 0) {
            return Err(DecodeError::InvalidData("AAC access unit extent"));
        }
        if RAW && raw_elements != self.slots.len() {
            return Err(DecodeError::InvalidData(
                "incomplete raw AAC channel layout",
            ));
        }
        if self.slots.is_empty() {
            return Err(DecodeError::InvalidData(
                "no channel configuration (missing PCE)",
            ));
        }
        self.finish_frame::<RAW>(out);
        Ok(layout_changed)
    }

    fn decode_sce(
        &mut self,
        br: &mut CachedBitReader<'_>,
        cfg: &CoreCfg,
        ei: usize,
        chan: usize,
    ) -> Result<()> {
        let e: &mut ElementState = &mut self.elements[ei];
        parse_ics(br, cfg, false, false, &mut e.cd[chan], &mut self.random)?;
        if let Some(p) = &mut e.ch[chan].pred {
            pred::apply(p, &mut e.cd[chan], cfg);
        }
        e.decoded = true;
        e.ch_decoded[chan] = true;
        Ok(())
    }

    fn decode_cpe(&mut self, br: &mut CachedBitReader<'_>, cfg: &CoreCfg, ei: usize) -> Result<()> {
        let e: &mut ElementState = &mut self.elements[ei];
        let common = br.bit();
        let mut ms_present = 0u8;
        let mut ms_mask = [false; 8 * MAX_SFB];
        if common {
            let mut info = e.cd[0].info;
            parse_ics_info(br, cfg, true, &mut info)?;
            e.cd[0].info = info;
            ms_present = br.read(2) as u8;
            match ms_present {
                0 => {}
                1 => {
                    for g in 0..info.num_window_groups {
                        for sfb in 0..info.max_sfb {
                            ms_mask[g * MAX_SFB + sfb] = br.bit();
                        }
                    }
                }
                2 => {
                    for g in 0..info.num_window_groups {
                        for sfb in 0..info.max_sfb {
                            ms_mask[g * MAX_SFB + sfb] = true;
                        }
                    }
                }
                _ => return Err(DecodeError::InvalidData("ms_mask_present 3")),
            }
            let mut info1 = info;
            // the right channel's LTP data is the second set
            info1.ltp[0] = info.ltp[1];
            e.cd[1].info = info1;
        }
        let (a, b) = e.cd.split_at_mut(1);
        parse_ics(br, cfg, common, false, &mut a[0], &mut self.random)?;
        parse_ics(br, cfg, common, false, &mut b[0], &mut self.random)?;
        if common && ms_present != 0 {
            apply_ms(&mut a[0], &mut b[0], &ms_mask);
        }
        if cfg.aot == AOT_MAIN {
            if let Some(p) = &mut e.ch[0].pred {
                pred::apply(p, &mut a[0], cfg);
            }
            if let Some(p) = &mut e.ch[1].pred {
                pred::apply(p, &mut b[0], cfg);
            }
        }
        apply_intensity(
            &a[0],
            &mut b[0],
            if common { ms_present } else { 0 },
            &ms_mask,
        );
        e.decoded = true;
        e.ch_decoded = [true, true];
        Ok(())
    }

    fn decode_sbr_ext(
        &mut self,
        br: &mut CachedBitReader<'_>,
        ei: usize,
        crc: bool,
        bits: usize,
        out: &mut AudioFrame,
        blocks: usize,
    ) -> Result<bool> {
        let is_cpe = self.elements[ei].kind == ID_CPE;
        // A mono stream whose very first frame carries SBR is output as stereo (FFmpeg assumes
        // HE-AAC v2 for implicitly signalled mono SBR).
        let to_stereo = self.frames_out == 0 && !self.ps_active && !is_cpe && self.slots.len() == 1;
        let channels = if to_stereo { 2 } else { self.layout.channels() };
        let mut changed = false;
        if !self.sbr_active {
            // implicit SBR signalling: FFmpeg switches the output to twice the core rate
            if SAMPLE_RATES[self.sf_index] > 48000 {
                // FFmpeg refuses SBR above 48 kHz core rate; ignore the payload
                return Ok(false);
            }
            // Every element runs SBR from now on; its state and the longer output are
            // charged before SBR starts.
            self.make_room(out, channels, SBR_FRAME_LEN, blocks)?;
            let mut sbr = Vec::with_capacity(self.elements.len());
            for e in &self.elements {
                sbr.push(match e.sbr {
                    None => Some(new_sbr(&self.budget, self.sf_index, e.kind)?),
                    Some(_) => None,
                });
            }
            for (e, s) in self.elements.iter_mut().zip(sbr) {
                if s.is_some() {
                    e.sbr = s;
                }
            }
            self.sbr_active = true;
            changed = true;
        } else if to_stereo {
            self.make_room(out, channels, self.frame_len(), blocks)?;
        }
        let e: &mut ElementState = &mut self.elements[ei];
        e.sbr
            .as_mut()
            .expect("every element runs SBR")
            .parse(br, crc, bits, is_cpe)?;
        if to_stereo {
            self.ps_active = true;
            self.rebuild_layout();
            changed = true;
        }
        Ok(changed)
    }

    #[cfg(feature = "inspect")]
    fn record_stats(&mut self) {
        self.stats.clear();
        for e in self.elements.iter().filter(|_| self.stats_enabled) {
            if !e.decoded {
                continue;
            }
            for c in 0..2 {
                if !e.ch_decoded[c] {
                    continue;
                }
                let cd = &e.cd[c];
                use std::fmt::Write;
                let _ = write!(
                    self.stats,
                    "el{}/{} ch{} seq{} sh{} maxsfb{} grp{} cb{:#x} tns{} pulse{} pred{} ltp{}; ",
                    e.kind,
                    e.tag,
                    c,
                    cd.info.window_sequence,
                    cd.info.window_shape,
                    cd.info.max_sfb,
                    cd.info.num_window_groups,
                    cd.cb_mask,
                    cd.tns.present as u8,
                    cd.pulse_used as u8,
                    cd.info.predictor_data_present as u8,
                    cd.info.ltp[0].present as u8
                );
            }
        }
        for c in self.cce.iter().filter(|_| self.stats_enabled) {
            use std::fmt::Write;
            let _ = write!(
                self.stats,
                "cce{} dec{} ind{} dom{} seq{} g1={:.4} tns{} cb{:#x} maxsfb{} tgts{:?}; ",
                c.tag,
                c.decoded as u8,
                c.ind_sw as u8,
                c.domain as u8,
                c.cd.info.window_sequence,
                c.gains[1][0],
                c.cd.tns.present as u8,
                c.cd.cb_mask,
                c.cd.info.max_sfb,
                c.targets
                    .iter()
                    .map(|t| (t.is_cpe, t.tag, t.l, t.r))
                    .collect::<Vec<_>>()
            );
        }
    }

    fn finish_frame<const RAW: bool>(&mut self, frame: &mut AudioFrame) {
        let cfg = CoreCfg {
            aot: self.aot,
            sf_index: self.sf_index,
        };
        #[cfg(feature = "inspect")]
        self.record_stats();
        crate::cce::process_cces(self);
        for ei in 0..self.elements.len() {
            let e: &mut ElementState = &mut self.elements[ei];
            if !RAW && !e.decoded {
                continue;
            }
            let (kind, tag) = (
                if !RAW && e.ch_decoded == [true, false] && e.kind == ID_CPE {
                    ID_SCE
                } else {
                    e.kind
                },
                e.bs_tag,
            );
            let channels = if RAW { e.nch } else { 2 };
            for c in 0..channels {
                if RAW || e.ch_decoded[c] {
                    let out = if RAW {
                        // Every fixed slot has its own element in wire order. The
                        // frame has already reserved a plane for each channel.
                        &mut frame.planes[self.plane_of[ei][c]][..]
                    } else {
                        &mut e.out[c][..]
                    };
                    let (cd, st) = (&mut e.cd[c], &mut e.ch[c]);
                    process_channel(&mut self.fb, &cfg, cd, st, out, &self.cce, kind, tag, c);
                } else if e.kind == ID_CPE {
                    for v in &mut e.out[c][..1024] {
                        *v = 0.0;
                    }
                }
            }
        }
        if RAW {
            return;
        }
        for ei in 0..self.elements.len() {
            let e: &mut ElementState = &mut self.elements[ei];
            if !e.decoded {
                // element missing in this frame: output silence for it
                for o in &mut e.out {
                    for v in o.iter_mut() {
                        *v = 0.0;
                    }
                }
                continue;
            }
            if self.sbr_active {
                let ps = self.ps_active && e.kind == ID_SCE;
                let nch = if e.kind == ID_CPE { 2 } else { 1 };
                let s = e.sbr.as_mut().unwrap();
                let (o0, o1) = e.out.split_at_mut(1);
                s.process(
                    &mut o0[0],
                    &mut o1[0],
                    nch,
                    ps,
                    e.kind == ID_CPE,
                    &mut self.sbr_dct,
                );
                if ps {
                    e.nch = 2;
                }
            }
        }
    }

    /// Charge the output of `blocks` raw data blocks in the configured layout before any is
    /// decoded. `emit_planes` grows the frame again only when the access unit reconfigures
    /// the decoder in band (a PCE, a CPE in a mono stream, the start of SBR or PS), which
    /// creates state when it appears.
    pub(crate) fn reserve_output<const RAW: bool>(
        &mut self,
        out: &mut AudioFrame,
        blocks: usize,
    ) -> Result<()> {
        let (channels, len) = (self.layout.channels(), blocks * self.frame_len());
        if RAW {
            // Charge caller-provided capacity before advancing signal history.
            return Ok(out.try_resize(channels, len, &mut self.output_charge)?);
        }
        if out.planes.len() < channels || out.planes[..channels].iter().any(|p| p.capacity() < len)
        {
            out.try_resize(channels, len, &mut self.output_charge)?;
            out.clear();
        }
        Ok(())
    }

    /// Append this block's output (frame_len samples per plane) in layout order; planes
    /// without a decoded element stay silent.
    pub(crate) fn emit_planes(&mut self, out: &mut AudioFrame) -> Result<()> {
        let flen = self.frame_len();
        let channels = self.layout.channels();
        if out.planes.len() != channels {
            out.clear(); // a mid-frame layout change restarts collection
        }
        let base = out.planes.first().map_or(0, Vec::len);
        out.try_resize(channels, base + flen, &mut self.output_charge)?;
        for (si, slot) in self.slots.iter().enumerate() {
            let Some(e) = self
                .elements
                .iter()
                .find(|e| e.kind == slot.kind && e.tag == slot.tag)
            else {
                continue;
            };
            for c in 0..2 {
                let p = self.plane_of[si][c];
                if let Some(dst) = out.planes.get_mut(p) {
                    // (a later slot mapped to the same plane replaces the earlier one)
                    dst[base..].copy_from_slice(&e.out[c][..flen]);
                }
            }
        }
        Ok(())
    }
}

/// Per-channel processing after spectral decoding and stereo tools:
/// prediction / LTP / coupling / TNS / filterbank.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_channel(
    fb: &mut Filterbank,
    cfg: &CoreCfg,
    cd: &mut ChannelData,
    st: &mut ChannelState,
    out: &mut [f32],
    cce: &[Charged<Box<crate::cce::CceState>>],
    kind: u8,
    tag: u8,
    ch_in_elem: usize,
) {
    if let Some(l) = &mut st.ltp {
        ltp::apply(l, fb, cd, st.prev_shape, cfg);
    }
    crate::cce::apply_spectral(cce, kind, tag, ch_in_elem, cd, false);
    apply_tns(cd, cfg);
    crate::cce::apply_spectral(cce, kind, tag, ch_in_elem, cd, true);
    let info = cd.info;
    fb.synth(
        &cd.coef,
        info.window_sequence,
        info.window_shape,
        st.prev_shape,
        &mut st.overlap,
        out,
    );
    if let Some(l) = &mut st.ltp {
        ltp::update(l, out, &st.overlap);
    }
    crate::cce::apply_time(cce, kind, tag, ch_in_elem, &mut out[..1024]);
    st.prev_shape = info.window_shape;
    st.prev_seq = info.window_sequence;
}

/// Assign speakers to the fixed elements of ISO/IEC 14496-3:2009/Amd.4, Table 1.19.
fn fixed_config_slots(cfg: u8, layout: FixedLayout) -> Vec<Slot> {
    use Speaker::{
        BackCenter, BackLeft, BackRight, FrontCenter, FrontLeft, FrontLeftOfCenter, FrontRight,
        FrontRightOfCenter, LowFrequency, SideLeft, SideRight, TopFrontLeft, TopFrontRight,
    };
    let sce = |s| Slot {
        kind: ID_SCE,
        tag: 0,
        speakers: [Some(s), None],
    };
    let cpe = |a, b| Slot {
        kind: ID_CPE,
        tag: 0,
        speakers: [Some(a), Some(b)],
    };
    let lfe = Slot {
        kind: ID_LFE,
        tag: 0,
        speakers: [Some(LowFrequency), None],
    };
    let surround_pair = || match layout {
        FixedLayout::Raw(Policy::ConventionalSurround) => cpe(BackLeft, BackRight),
        _ => cpe(SideLeft, SideRight),
    };
    let mut v = match cfg {
        1 => vec![sce(FrontCenter)],
        2 => vec![cpe(FrontLeft, FrontRight)],
        3 => vec![sce(FrontCenter), cpe(FrontLeft, FrontRight)],
        4 => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            sce(BackCenter),
        ],
        5 => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            surround_pair(),
        ],
        6 => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            surround_pair(),
            lfe,
        ],
        7 if matches!(layout, FixedLayout::Raw(Policy::Specification)) => vec![
            sce(FrontCenter),
            cpe(FrontLeftOfCenter, FrontRightOfCenter),
            cpe(FrontLeft, FrontRight),
            cpe(SideLeft, SideRight),
            lfe,
        ],
        11 => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            surround_pair(),
            sce(BackCenter),
            lfe,
        ],
        14 => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            surround_pair(),
            lfe,
            cpe(TopFrontLeft, TopFrontRight),
        ],
        _ => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            cpe(SideLeft, SideRight),
            cpe(BackLeft, BackRight),
            lfe,
        ],
    };
    // distinct tags per kind (by order) so element states are separate
    let mut cnt = [0u8; 4];
    for s in &mut v {
        s.tag = cnt[s.kind as usize];
        cnt[s.kind as usize] += 1;
    }
    v
}

/// Speaker assignment for a PCE, reproducing FFmpeg 9's observable mapping (probed with
/// `harness/codecs/aac/tools/probe_layout.py`). Returns the slots and whether the output keeps
/// PCE order (FFmpeg "unknown" layouts) instead of native order.
fn pce_slots(p: &Pce, adts: bool) -> (Vec<Slot>, bool) {
    use Speaker::{
        BackCenter, BackLeft, BackRight, BottomFrontCenter, BottomFrontLeft, BottomFrontRight,
        FrontCenter, FrontLeft, FrontLeftOfCenter, FrontRight, FrontRightOfCenter, LowFrequency,
        LowFrequency2, SideLeft, SideRight, TopBackCenter, TopBackLeft, TopBackRight, TopCenter,
        TopFrontCenter, TopFrontLeft, TopFrontRight, TopSideLeft, TopSideRight,
    };
    let mut slots = Vec::new();
    let mut unordered = false;
    let normal = |v: &[crate::pce::PceElement]| -> Vec<crate::pce::PceElement> {
        v.iter().filter(|e| e.height == 0).copied().collect()
    };
    let front = normal(&p.front);
    let side = normal(&p.side);
    let back = normal(&p.back);
    let kinds =
        |v: &[crate::pce::PceElement]| -> Vec<bool> { v.iter().map(|e| e.is_cpe).collect() };
    let no_heights = p
        .front
        .iter()
        .chain(&p.side)
        .chain(&p.back)
        .all(|e| e.height == 0);
    // ADTS uses the conventional side pair for the 5.0/5.1 pattern. An explicit
    // program in AudioSpecificConfig keeps its declared back pair.
    let cfg56 = adts
        && no_heights
        && kinds(&front) == [false, true]
        && side.is_empty()
        && kinds(&back) == [true]
        && p.lfe.len() <= 1;
    let sce = |t, s| Slot {
        kind: ID_SCE,
        tag: t,
        speakers: [Some(s), None],
    };
    let cpe = |t, a, b| Slot {
        kind: ID_CPE,
        tag: t,
        speakers: [Some(a), Some(b)],
    };
    // front
    let nfch: usize = front.iter().map(|e| if e.is_cpe { 2 } else { 1 }).sum();
    let mut rest: &[crate::pce::PceElement] = &front;
    if nfch % 2 == 1
        && let Some(e) = front.first()
    {
        if !e.is_cpe {
            slots.push(sce(e.tag, FrontCenter));
            rest = &front[1..];
        } else {
            unordered = true;
        }
    }
    // group into pairs (CPE, or two consecutive SCEs)
    enum Pair {
        Cpe(u8),
        Sces(u8, u8),
    }
    let mut pairs: Vec<Pair> = Vec::new();
    let mut k = 0;
    while k < rest.len() {
        if rest[k].is_cpe {
            pairs.push(Pair::Cpe(rest[k].tag));
            k += 1;
        } else if k + 1 < rest.len() && !rest[k + 1].is_cpe {
            pairs.push(Pair::Sces(rest[k].tag, rest[k + 1].tag));
            k += 2;
        } else {
            unordered = true;
            slots.push(sce(rest[k].tag, FrontCenter));
            k += 1;
        }
    }
    if pairs.len() > 2 {
        unordered = true;
    }
    for (pi, pr) in pairs.iter().enumerate() {
        let (a, b) = if pairs.len() >= 2 && pi == 0 {
            (FrontLeftOfCenter, FrontRightOfCenter)
        } else {
            (FrontLeft, FrontRight)
        };
        match *pr {
            Pair::Cpe(t) => slots.push(cpe(t, a, b)),
            Pair::Sces(t0, t1) => {
                slots.push(sce(t0, a));
                slots.push(sce(t1, b));
            }
        }
    }
    // side
    let mut k = 0;
    while k < side.len() {
        if side[k].is_cpe {
            slots.push(cpe(side[k].tag, SideLeft, SideRight));
            k += 1;
        } else if k + 1 < side.len() && !side[k + 1].is_cpe {
            slots.push(sce(side[k].tag, SideLeft));
            slots.push(sce(side[k + 1].tag, SideRight));
            k += 2;
        } else {
            unordered = true;
            slots.push(sce(side[k].tag, SideLeft));
            k += 1;
        }
    }
    // back
    let mut back_pairs = back.iter().filter(|e| e.is_cpe).count();
    if !adts {
        back_pairs += back
            .split(|e| e.is_cpe)
            .map(|singles| singles.len() / 2)
            .sum::<usize>();
    }
    let mut first_back_pair_to_side = side.is_empty() && (back_pairs >= 2 || cfg56);
    let mut back_speakers = |can_use_side| {
        if first_back_pair_to_side && can_use_side {
            first_back_pair_to_side = false;
            (SideLeft, SideRight)
        } else {
            (BackLeft, BackRight)
        }
    };
    let mut k = 0;
    while k < back.len() {
        if back[k].is_cpe {
            let (left, right) = back_speakers(true);
            slots.push(cpe(back[k].tag, left, right));
            k += 1;
        } else if k + 1 < back.len() && !back[k + 1].is_cpe {
            let (left, right) = back_speakers(!adts);
            slots.push(sce(back[k].tag, left));
            slots.push(sce(back[k + 1].tag, right));
            k += 2;
        } else {
            slots.push(sce(back[k].tag, BackCenter));
            k += 1;
        }
    }
    for (k, &t) in p.lfe.iter().enumerate() {
        slots.push(Slot {
            kind: ID_LFE,
            tag: t,
            speakers: [
                Some(if k == 0 { LowFrequency } else { LowFrequency2 }),
                None,
            ],
        });
    }
    // height layers
    for e in p.front.iter().filter(|e| e.height == 1) {
        slots.push(if e.is_cpe {
            cpe(e.tag, TopFrontLeft, TopFrontRight)
        } else {
            sce(e.tag, TopFrontCenter)
        });
    }
    for e in p.side.iter().filter(|e| e.height == 1) {
        slots.push(if e.is_cpe {
            cpe(e.tag, TopSideLeft, TopSideRight)
        } else {
            sce(e.tag, TopCenter)
        });
    }
    for e in p.back.iter().filter(|e| e.height == 1) {
        slots.push(if e.is_cpe {
            cpe(e.tag, TopBackLeft, TopBackRight)
        } else {
            sce(e.tag, TopBackCenter)
        });
    }
    for e in p
        .front
        .iter()
        .chain(&p.side)
        .chain(&p.back)
        .filter(|e| e.height == 2)
    {
        slots.push(if e.is_cpe {
            cpe(e.tag, BottomFrontLeft, BottomFrontRight)
        } else {
            sce(e.tag, BottomFrontCenter)
        });
    }
    if unordered {
        // FFmpeg falls back to an unspecified layout in PCE element order
        let mut ordered = Vec::new();
        for e in p.front.iter().chain(&p.side).chain(&p.back) {
            let kind = if e.is_cpe { ID_CPE } else { ID_SCE };
            if let Some(s) = slots.iter().find(|s| s.kind == kind && s.tag == e.tag) {
                ordered.push(s.clone());
            }
        }
        for s in &slots {
            if s.kind == ID_LFE {
                ordered.push(s.clone());
            }
        }
        slots = ordered;
    }
    (slots, unordered)
}

/// sscanf("libfaac %d.%d") == 2 equivalent.
fn is_libfaac_signature(t: &[u8]) -> bool {
    let Some(rest) = t.strip_prefix(b"libfaac") else {
        return false;
    };
    let mut i = 0;
    let skip_ws = |i: &mut usize| {
        while *i < rest.len() && matches!(rest[*i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
            *i += 1;
        }
    };
    let int = |i: &mut usize| -> bool {
        if *i < rest.len() && (rest[*i] == b'+' || rest[*i] == b'-') {
            *i += 1;
        }
        let s = *i;
        while *i < rest.len() && rest[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > s
    };
    skip_ws(&mut i);
    if !int(&mut i) {
        return false;
    }
    if i >= rest.len() || rest[i] != b'.' {
        return false;
    }
    i += 1;
    skip_ws(&mut i);
    int(&mut i)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cce::CceState;

    /// `value`'s `Debug` text without the counters of the pools it is charged to.
    pub(crate) fn without_pools(value: &impl core::fmt::Debug) -> String {
        let text = format!("{value:?}");
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(at) = rest.find("Budget(") {
            out.push_str(&rest[..at]);
            let end = rest[at..].find("})").map_or(rest.len(), |e| at + e + 2);
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    /// The bytes `make` leaves charged to a fresh pool while its result lives.
    fn charge_of<T>(make: impl FnOnce(&Budget) -> Result<T>) -> usize {
        let budget = Budget::new(1 << 30).unwrap();
        let idle = budget.used();
        let value = make(&budget).unwrap();
        let used = budget.used() - idle;
        drop(value);
        used
    }

    #[test]
    fn a_renewed_element_is_a_new_one() {
        let budget = Budget::new(1 << 30).unwrap();
        for aot in [AOT_LC, AOT_MAIN, AOT_LTP] {
            for kind in [ID_SCE, ID_CPE] {
                let fresh = || {
                    let mut e = ElementState::new(&budget, aot, kind, 3).unwrap();
                    e.sbr = Some(new_sbr(&budget, 4, kind).unwrap());
                    e
                };
                let mut e = fresh();
                e.ch[0].overlap.fill(1.0);
                e.ch[1].prev_shape = 1;
                e.out[1][7] = 2.0;
                (e.decoded, e.nch, e.bs_tag, e.ch_decoded) = (true, 2, 9, [true; 2]);
                e.cd[0].global_gain = 120;
                e.renew();
                assert_eq!(without_pools(&e), without_pools(&fresh()), "{aot} {kind}");
            }
        }
    }

    #[test]
    fn the_default_limit_holds_a_switch_between_the_largest_layouts() {
        // An element is a kind and a 4-bit tag, so a PCE can list 45 CPEs but has at most
        // 16 distinct ones, and two layouts together hold at most 48 elements (16 SCEs, 16
        // CPEs, 16 LFEs): a switch reuses what both have. Each element here is the largest
        // one: it carries SBR (with PS's synthesis where it applies) in the kind and profile
        // with the largest state.
        let element = [AOT_MAIN, AOT_LTP]
            .into_iter()
            .flat_map(|aot| [ID_SCE, ID_CPE].map(|kind| (aot, kind)))
            .map(|(aot, kind)| {
                charge_of(|b| {
                    let mut e = Charged::boxed(b, ElementState::new(b, aot, kind, 0)?)?;
                    e.sbr = Some(Charged::boxed(
                        b,
                        sbr::Sbr::new(48_000, kind == ID_CPE, b)?,
                    )?);
                    Ok(e)
                })
            })
            .max()
            .unwrap();
        let cce = charge_of(|b| Ok(Charged::boxed(b, CceState::new(b, 0)?)?));
        // Four raw data blocks of 2048 samples (SBR) in each plane.
        let output = charge_of(|b| {
            let (mut frame, mut charge) = (AudioFrame::default(), Reservation::empty(b));
            frame.try_resize(ChannelLayout::CAPACITY, 4 * 2048, &mut charge)?;
            Ok((frame, charge))
        });
        let limit = crate::Limits::default().live_storage_bytes;
        let pool = Budget::new(limit).unwrap().used();
        let worst = pool + 48 * element + 16 * cce + output;
        eprintln!("element {element}, cce {cce}, output {output}, worst {worst}");
        assert!(worst <= limit, "{worst} > {limit}");
    }
}
