//! raw_data_block() decoding: element parsing, channel mapping, per-channel state and
//! output assembly.

use crate::adts::AdtsHeader;
use crate::bits::BitReader;
use crate::error::{DecodeError, Feature, Result};
use crate::filterbank::Filterbank;
use crate::ics::*;
use crate::layout::{ChannelLayout, Speaker};
use crate::pce::{Pce, parse_pce};
use crate::tables::SAMPLE_RATES;
use crate::{ltp, pred, sbr};

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
const PNS_INITIAL_STATE: u32 = 0x1f2e3d4c;

/// Persistent per-channel state.
pub(crate) struct ChannelState {
    pub overlap: Vec<f32>,
    pub prev_shape: u8,
    pub prev_seq: u8,
    pub pred: Option<Box<pred::PredState>>,
    pub ltp: Option<Box<ltp::LtpState>>,
}

impl ChannelState {
    fn new(aot: u8) -> ChannelState {
        ChannelState {
            overlap: vec![0.0; 1024],
            prev_shape: 0,
            prev_seq: 0,
            pred: if aot == AOT_MAIN {
                Some(Box::new(pred::PredState::new()))
            } else {
                None
            },
            ltp: if aot == AOT_LTP {
                Some(Box::new(ltp::LtpState::new()))
            } else {
                None
            },
        }
    }
}

/// One output slot: syntactic element + speakers of its channel(s).
#[derive(Clone, Debug)]
pub(crate) struct Slot {
    pub kind: u8,
    pub tag: u8,
    pub speakers: [Option<Speaker>; 2],
}

/// Element instance state (channels + SBR).
pub(crate) struct ElementState {
    pub kind: u8,
    pub tag: u8,
    pub ch: [ChannelState; 2],
    pub sbr: Option<Box<sbr::Sbr>>,
    /// Time output of the core for this frame (per channel, 1024 samples), and after SBR/PS
    /// up to 2048.
    pub out: [Vec<f32>; 2],
    /// Set when the element was decoded in the current raw_data_block.
    pub decoded: bool,
    /// The element's channel count (2 for a mono SBR element carrying PS).
    pub nch: usize,
    /// Spectral data of the current frame (filterbank deferred to the END element).
    pub cd: [Box<ChannelData>; 2],
    /// element_instance_tag as coded in the bitstream this block (CCE targets refer to it).
    pub bs_tag: u8,
    /// Which channels were decoded this block (an SCE may fill one channel of a CPE slot).
    pub ch_decoded: [bool; 2],
}

pub(crate) struct Core {
    pub aot: u8,
    pub sf_index: usize,
    pub channel_config: u8,
    pub slots: Vec<Slot>,
    pub layout: ChannelLayout,
    /// plane index for slot i channel c
    pub plane_of: Vec<[usize; 2]>,
    pub elements: Vec<ElementState>,
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
    pub cce: Vec<crate::cce::CceState>,
    /// Debug statistics of the last block: one line per decoded element (only collected
    /// when `stats_enabled`).
    pub stats: String,
    pub stats_enabled: bool,
    /// Output samples (per channel) still to be dropped (FFmpeg skip_samples behaviour).
    pub skip_samples: usize,
    /// Frames output since the last (re)configuration.
    pub frames_out: u64,
    /// PS may switch a mono output to stereo after the first frame (not FFmpeg behaviour).
    pub ps_upgrade_allowed: bool,
    /// Debug: bit ranges (relative to the payload) of SBR fill elements of the last frame.
    pub sbr_fil_ranges: Vec<(usize, usize)>,
}

impl Core {
    pub(crate) fn new() -> Core {
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
            stats: String::new(),
            stats_enabled: false,
            skip_samples: 0,
            sbr_fil_ranges: Vec::new(),
            frames_out: 0,
            ps_upgrade_allowed: false,
        }
    }

    pub(crate) fn reset_history(&mut self) {
        self.random = PNS_INITIAL_STATE;
        for e in &mut self.elements {
            for c in &mut e.ch {
                *c = ChannelState::new(self.aot);
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
        if self.sbr_active { 2048 } else { 1024 }
    }

    /// (Re)configure from an ADTS header. Returns true when the configuration changed.
    pub(crate) fn configure_adts(&mut self, h: &AdtsHeader) -> Result<bool> {
        let aot = h.profile + 1;
        match aot {
            AOT_MAIN | AOT_LC | AOT_LTP => {}
            3 => return Err(DecodeError::Unsupported(Feature::SsrGainControl)),
            _ => return Err(DecodeError::Unsupported(Feature::ObjectType(aot))),
        }
        if h.sf_index > 12 {
            return Err(DecodeError::Unsupported(Feature::SampleRateIndex(
                h.sf_index,
            )));
        }
        let changed = !self.configured
            || aot != self.aot
            || h.sf_index as usize != self.sf_index
            || h.channel_config != self.channel_config;
        if !changed {
            return Ok(false);
        }
        self.aot = aot;
        self.sf_index = h.sf_index as usize;
        self.channel_config = h.channel_config;
        self.sbr_active = false;
        self.ps_active = false;
        self.frames_out = 0;
        self.elements.clear();
        self.cce.clear();
        self.configured = true;
        self.unordered = false;
        if h.channel_config != 0 {
            self.slots = fixed_config_slots(h.channel_config);
            self.pce = None;
        } else {
            self.slots.clear();
        }
        self.rebuild_layout();
        Ok(true)
    }

    pub(crate) fn rebuild_layout(&mut self) {
        // Expand slots to planes; PS doubles the single mono slot.
        let mut entries: Vec<(Speaker, usize, usize)> = Vec::new();
        for (i, s) in self.slots.iter().enumerate() {
            let mut sp = s.speakers;
            if self.ps_active && s.kind == ID_SCE && self.slots.len() == 1 {
                sp = [Some(Speaker::FrontLeft), Some(Speaker::FrontRight)];
            }
            for (c, x) in sp.iter().enumerate() {
                if let Some(x) = x {
                    entries.push((*x, i, c));
                }
            }
        }
        // stable sort by speaker id: FFmpeg native order
        if !self.unordered {
            entries.sort_by_key(|e| e.0);
        }
        self.plane_of = vec![[usize::MAX; 2]; self.slots.len()];
        let mut sp = Vec::new();
        for (p, e) in entries.iter().enumerate() {
            self.plane_of[e.1][e.2] = p;
            sp.push(e.0);
        }
        self.layout = ChannelLayout::from_speakers(&sp);
    }

    fn element_index(&mut self, kind: u8, tag: u8, occurrence: usize) -> Option<usize> {
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
        }?;
        let s = &self.slots[slot];
        if let Some(e) = self
            .elements
            .iter()
            .position(|e| e.kind == s.kind && e.tag == s.tag)
        {
            return Some(e);
        }
        let (k, t) = (s.kind, s.tag);
        self.elements.push(ElementState {
            kind: k,
            tag: t,
            ch: [ChannelState::new(self.aot), ChannelState::new(self.aot)],
            sbr: None,
            out: [vec![0.0; 2048], vec![0.0; 2048]],
            decoded: false,
            nch: if k == ID_CPE { 2 } else { 1 },
            cd: [Box::new(ChannelData::new()), Box::new(ChannelData::new())],
            ch_decoded: [false; 2],
            bs_tag: t,
        });
        Some(self.elements.len() - 1)
    }

    /// Decode one raw_data_block(); appends `frame_len()` samples per plane to `planes`.
    pub(crate) fn decode_block(&mut self, br: &mut BitReader, block_start: usize) -> Result<bool> {
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
        let mut last_elem: Option<usize> = None;
        let mut layout_changed = false;
        let mut pending_cce: Vec<usize> = Vec::new();
        loop {
            if br.left() < 3 {
                return Err(DecodeError::InvalidData("missing END element"));
            }
            let id = br.read(3) as u8;
            match id {
                ID_SCE | ID_LFE | ID_CPE => {
                    let tag = br.read(4) as u8;
                    let o = occ[id as usize];
                    occ[id as usize] += 1;
                    let mut ei = self.element_index(id, tag, o);
                    if ei.is_none() && self.channel_config == 2 && id == ID_SCE && o < 2 {
                        // FFmpeg maps SCEs of a channel_configuration 2 stream to FL then FR
                        // (probed); the CPE slot's channel o carries them.
                        if let Some(i) = self.element_index(ID_CPE, 0, 0) {
                            self.elements[i].bs_tag = tag;
                            self.decode_sce(br, &cfg, i, o)?;
                            last_elem = Some(i);
                            continue;
                        }
                    }
                    if ei.is_none() && self.channel_config == 1 && id == ID_CPE && o == 0 {
                        // FFmpeg switches a channel_configuration 1 stream to stereo when a
                        // CPE appears (probed).
                        self.slots = fixed_config_slots(2);
                        self.elements.clear();
                        self.channel_config = 2;
                        self.rebuild_layout();
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
                    let ci = crate::cce::parse_cce(self, br, &cfg)?;
                    pending_cce.push(ci);
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
                    if self.channel_config == 0 && self.pce.as_ref() != Some(&p) {
                        {
                            let (slots, unordered) = pce_slots(&p);
                            self.slots = slots;
                            self.unordered = unordered;
                            self.pce = Some(p);
                            self.elements.clear();
                            self.cce.clear();
                            self.rebuild_layout();
                            layout_changed = true;
                        }
                    }
                    last_elem = None;
                }
                ID_FIL => {
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
                        if ext == 0 && count * 8 - 4 >= 13 + 7 * 8 {
                            // EXT_FILL carrying an encoder signature. FFmpeg skips the first
                            // 1024 output samples when it reads "libfaac <major>.<minor>" here
                            // (faac encoder delay compensation); replicated for output parity.
                            let mut r = br.clone();
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
                            self.sbr_fil_ranges.push((fil_start, end));
                            let saved_end = br.end();
                            br.set_end(end);
                            let r =
                                self.decode_sbr_ext(br, ei, ext == EXT_SBR_DATA_CRC, count * 8 - 4);
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
        if self.slots.is_empty() {
            return Err(DecodeError::InvalidData(
                "no channel configuration (missing PCE)",
            ));
        }
        // finish: SBR/PS + coupling + output
        self.finish_frame(&pending_cce)?;
        Ok(layout_changed)
    }

    fn decode_sce(
        &mut self,
        br: &mut BitReader,
        cfg: &CoreCfg,
        ei: usize,
        chan: usize,
    ) -> Result<()> {
        let e = &mut self.elements[ei];
        parse_ics(br, cfg, false, false, &mut e.cd[chan], &mut self.random)?;
        if let Some(p) = &mut e.ch[chan].pred {
            pred::apply(p, &mut e.cd[chan], cfg);
        }
        e.decoded = true;
        e.ch_decoded[chan] = true;
        Ok(())
    }

    fn decode_cpe(&mut self, br: &mut BitReader, cfg: &CoreCfg, ei: usize) -> Result<()> {
        let e = &mut self.elements[ei];
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
        br: &mut BitReader,
        ei: usize,
        crc: bool,
        bits: usize,
    ) -> Result<bool> {
        let rate = SAMPLE_RATES[self.sf_index];
        let mut changed = false;
        if !self.sbr_active {
            // implicit SBR signalling: FFmpeg switches the output to twice the core rate
            if rate > 48000 {
                // FFmpeg refuses SBR above 48 kHz core rate; ignore the payload
                return Ok(false);
            }
            self.sbr_active = true;
            changed = true;
        }
        let e = &mut self.elements[ei];
        if e.sbr.is_none() {
            e.sbr = Some(Box::new(sbr::Sbr::new(rate)));
        }
        let is_cpe = e.kind == ID_CPE;
        let s = e.sbr.as_mut().unwrap();
        let ps_found = s.parse(br, crc, bits, is_cpe)?;
        // A mono stream whose very first frame carries SBR is output as stereo (FFmpeg assumes
        // HE-AAC v2 for implicitly signalled mono SBR); later PS data is applied when it appears.
        let first_frame = self.frames_out == 0;
        if (ps_found || first_frame)
            && !self.ps_active
            && !is_cpe
            && self.slots.len() == 1
            && (first_frame || ps_found && self.ps_upgrade_allowed)
        {
            self.ps_active = true;
            self.rebuild_layout();
            changed = true;
        }
        Ok(changed)
    }

    fn finish_frame(&mut self, pending_cce: &[usize]) -> Result<()> {
        let cfg = CoreCfg {
            aot: self.aot,
            sf_index: self.sf_index,
        };
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
        crate::cce::process_cces(self);
        for ei in 0..self.elements.len() {
            let e = &mut self.elements[ei];
            if !e.decoded {
                continue;
            }
            let (kind, tag) = (
                if e.ch_decoded == [true, false] && e.kind == ID_CPE {
                    ID_SCE
                } else {
                    e.kind
                },
                e.bs_tag,
            );
            for c in 0..2 {
                if e.ch_decoded[c] {
                    let (cd, st, out) = (&mut e.cd[c], &mut e.ch[c], &mut e.out[c]);
                    process_channel(&mut self.fb, &cfg, cd, st, out, &mut self.cce, kind, tag, c);
                } else if e.kind == ID_CPE {
                    for v in e.out[c][..1024].iter_mut() {
                        *v = 0.0;
                    }
                }
            }
        }
        let _ = pending_cce;
        for ei in 0..self.elements.len() {
            let e = &mut self.elements[ei];
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
                let rate = SAMPLE_RATES[self.sf_index];
                if e.sbr.is_none() {
                    e.sbr = Some(Box::new(sbr::Sbr::new(rate)));
                }
                let ps = self.ps_active && e.kind == ID_SCE;
                let nch = if e.kind == ID_CPE { 2 } else { 1 };
                let s = e.sbr.as_mut().unwrap();
                let (o0, o1) = e.out.split_at_mut(1);
                s.process(&mut o0[0], &mut o1[0], nch, ps, e.kind == ID_CPE);
                if ps {
                    e.nch = 2;
                }
            }
        }
        Ok(())
    }

    /// Append this block's output (frame_len samples per plane) in layout order.
    pub(crate) fn emit_planes(&self, planes: &mut Vec<Vec<f32>>) {
        let flen = self.frame_len();
        if planes.len() != self.layout.channels() {
            planes.clear();
            planes.resize_with(self.layout.channels(), Vec::new);
        }
        let base = planes.first().map_or(0, |p| p.len());
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
                if let Some(dst) = planes.get_mut(p) {
                    // (a later slot mapped to the same plane replaces the earlier one)
                    dst.truncate(base);
                    dst.extend_from_slice(&e.out[c][..flen]);
                }
            }
        }
        // planes without a decoded element stay silent
        for p in planes.iter_mut() {
            p.resize(base + flen, 0.0);
        }
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
    cce: &mut [crate::cce::CceState],
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

/// Element layout of channel_configuration 1..7 (ISO/IEC 14496-3 Table 1.19) with the
/// speaker positions FFmpeg assigns.
pub(crate) fn fixed_config_slots(cfg: u8) -> Vec<Slot> {
    use Speaker::*;
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
    let mut v = match cfg {
        1 => vec![sce(FrontCenter)],
        2 => vec![cpe(FrontLeft, FrontRight)],
        3 => vec![sce(FrontCenter), cpe(FrontLeft, FrontRight)],
        4 => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            sce(BackCenter),
        ],
        // FFmpeg 9 maps configurations 5/6 to 5.0/5.1 (side) and 7 to 7.1 (probed)
        5 => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            cpe(SideLeft, SideRight),
        ],
        6 => vec![
            sce(FrontCenter),
            cpe(FrontLeft, FrontRight),
            cpe(SideLeft, SideRight),
            lfe,
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
/// research/d4/aac/tools/probe_layout.py, see STATUS.md). Returns the slots and whether the
/// output keeps PCE order (FFmpeg "unknown" layouts) instead of native order.
pub(crate) fn pce_slots(p: &Pce) -> (Vec<Slot>, bool) {
    use Speaker::*;
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
    // Element pattern of channel_configuration 5/6 (front SCE+CPE, back CPE, <=1 LFE):
    // FFmpeg uses the configuration's 5.0/5.1 (side) layout.
    let cfg56 = no_heights
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
    if nfch % 2 == 1 {
        if let Some(e) = front.first() {
            if !e.is_cpe {
                slots.push(sce(e.tag, FrontCenter));
                rest = &front[1..];
            } else {
                unordered = true;
            }
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
    let back_pairs = back.iter().filter(|e| e.is_cpe).count();
    let mut first_back_pair_to_side = side.is_empty() && (back_pairs >= 2 || cfg56);
    let mut k = 0;
    while k < back.len() {
        if back[k].is_cpe {
            if first_back_pair_to_side {
                slots.push(cpe(back[k].tag, SideLeft, SideRight));
                first_back_pair_to_side = false;
            } else {
                slots.push(cpe(back[k].tag, BackLeft, BackRight));
            }
            k += 1;
        } else if k + 1 < back.len() && !back[k + 1].is_cpe {
            slots.push(sce(back[k].tag, BackLeft));
            slots.push(sce(back[k + 1].tag, BackRight));
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
