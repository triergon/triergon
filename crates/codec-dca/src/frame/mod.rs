//! Frame decoding stages: core frame and channel set headers (Tables 5-1, 5-21), core
//! subframes, optional information, and the core-embedded XCh / X96 extensions
//! (clause 6), followed by output synthesis.

mod synthesis;

use crate::bits::BitReader;
use crate::header::{FrameHeader, SetHeader};
use crate::subframe::{self, SubBuf, SubframeParams};
use crate::{AudioFrame, Config, DecodeError, Decoder, Speaker, x96};

/// Per-frame values shared by the decoding stages.
struct CoreFrame<'a> {
    h: FrameHeader,
    set: SetHeader,
    /// The frame bytes (FSIZE).
    data: &'a [u8],
    /// Coded channel order of the core channels.
    speakers: &'static [Speaker],
    nsubfs: usize,
    /// Subsubframe count of each subframe.
    sf_nssc: [usize; 16],
    /// The subband history of the previous frame continues.
    keep: bool,
    params: SubframeParams,
}

impl Decoder {
    pub(crate) fn decode_normalized(
        &mut self,
        data: &[u8],
        out: &mut AudioFrame,
    ) -> Result<(), DecodeError> {
        let (mut f, mut br) = self.begin_frame(data)?;
        let mut nsubs_all = [0u8; 8];
        nsubs_all[..f.set.nch].copy_from_slice(&f.set.nsubs[..f.set.nch]);
        self.decode_core(&mut br, &mut f, &nsubs_all)?;
        let core_end = optional_info(&mut br, &f);

        // Core-embedded extensions (clause 6).
        if f.h.ext_audio
            && f.h.ext_audio_id == 6
            && find_sync32(f.data, core_end, 0x4700_4A03).is_some()
        {
            // XXCH inside the core substream (clause 6.5) is not implemented: no selected
            // vector carries it (every 7.1 vector puts XXCH in the extension substream).
            return Err(DecodeError::Unsupported(
                "XXCH channel extension in the core substream",
            ));
        }
        let mut nslots = f.set.nch;
        let mut xch = false;
        if f.h.ext_audio
            && f.h.ext_audio_id == 0
            && let Some(pos) = find_xch(f.data, core_end)
        {
            self.decode_xch(&f, pos, &mut nsubs_all)?;
            nslots += 1;
            xch = true;
        }
        self.prev_xch = xch;
        let mut have_x96 = false;
        if f.h.ext_audio
            && f.h.ext_audio_id == 2
            && let Some(pos) = x96::find(f.data, core_end)
        {
            have_x96 = self.decode_x96(&f, pos)?;
        }

        self.prepare_synthesis(&f, nslots, have_x96);
        let (plane_of, lfe_plane) = synthesis::output_layout(out, &f, xch, have_x96);
        self.postprocess_subbands(&f, xch, have_x96)?;
        self.synthesize(out, &f, nslots, have_x96, &plane_of)?;
        if let Some(lp) = lfe_plane {
            self.synthesize_lfe(out, lp, &f, have_x96);
        }

        Ok(())
    }

    /// Frame header and primary audio coding header, and the history continuity rule.
    #[inline(always)]
    fn begin_frame<'a>(
        &mut self,
        data: &'a [u8],
    ) -> Result<(CoreFrame<'a>, BitReader<'a>), DecodeError> {
        let mut br = BitReader::new(data);
        let h = FrameHeader::parse(&mut br)?;
        if h.fsize > data.len() {
            return Err(DecodeError::InvalidData("frame shorter than FSIZE"));
        }
        let frame = &data[..h.fsize];
        let mut br = {
            let pos = br.position();
            let mut b = BitReader::new(frame);
            b.seek(pos);
            b
        };
        let speakers = amode_speakers(h.amode).ok_or(DecodeError::Unsupported(
            "AMODE above 9 (more than 5 core channels)",
        ))?;
        if h.short != 31 {
            // FFmpeg (reference) refuses deficit-sample frames.
            return Err(DecodeError::Unsupported("deficit samples (SHORT != 31)"));
        }
        let nsubfs = br.read(4) as usize + 1;
        let set = SetHeader::parse(&mut br, h.cpf)?;
        if set.nch != speakers.len() {
            return Err(DecodeError::InvalidData("PCHS does not match AMODE"));
        }
        let cfg = Config {
            amode: h.amode,
            nch: set.nch,
            lff: h.lff,
            filts: h.filts,
            rate: h.sample_rate,
        };
        let same = self.prev == Some(cfg);
        if !same {
            self.reset();
        }
        // FFmpeg (reference) loses the ADPCM and LFE histories whenever a frame needs more
        // subband samples than any frame since the last reset (observed with the
        // `lfe_mixed` / `size_changes_adpcm` synthetic streams).
        let grows = h.blocks > self.max_blocks;
        if grows {
            self.max_blocks = h.blocks;
            self.lfe_state.reset();
        }
        let keep = same && h.hflag && !grows;
        self.sub.start_frame(0..set.nch, h.blocks, keep);
        self.prev = Some(cfg);

        let params = SubframeParams {
            nssc: 0,
            aspf: h.aspf,
            cpf: h.cpf,
            lossless: h.rate == 0x1F,
            dynf: h.dynf,
            lff: h.lff,
        };
        let f = CoreFrame {
            h,
            set,
            data: frame,
            speakers,
            nsubfs,
            sf_nssc: [0; 16],
            keep,
            params,
        };
        Ok((f, br))
    }

    /// Core subframes of the primary channel set, with the decimated LFE samples.
    #[inline(always)]
    fn decode_core(
        &mut self,
        br: &mut BitReader<'_>,
        f: &mut CoreFrame<'_>,
        nsubs_all: &[u8; 8],
    ) -> Result<(), DecodeError> {
        let h = &f.h;
        let params_base = &f.params;
        let mut t0 = 0usize;
        let mut lfe_pos = 0usize;
        for sf in 0..f.nsubfs {
            let nssc = br.read(2) as usize + 1;
            // PSC (partial subsubframe sample count) is read and ignored, as FFmpeg does.
            let _psc = br.read(3);
            if t0 + nssc * 8 > h.blocks {
                return Err(DecodeError::InvalidData("subframes exceed NBLKS"));
            }
            f.sf_nssc[sf] = nssc;
            let p = SubframeParams {
                nssc,
                ..*params_base
            };
            let lfe = if h.lff > 0 {
                Some(&mut self.lfe_dec[lfe_pos..])
            } else {
                None
            };
            subframe::decode_subframe(
                br,
                &f.set,
                &mut self.side,
                &p,
                &mut self.sub,
                0,
                t0,
                lfe,
                nsubs_all,
            )?;
            t0 += nssc * 8;
            lfe_pos += 2 * h.lff as usize * nssc;
        }
        if t0 != h.blocks {
            return Err(DecodeError::InvalidData("subframes do not fill NBLKS"));
        }
        Ok(())
    }

    /// XCh extension (clause 6.4): one extra channel decoded into the slot after the core.
    #[inline(always)]
    fn decode_xch(
        &mut self,
        f: &CoreFrame<'_>,
        pos: usize,
        nsubs_all: &mut [u8; 8],
    ) -> Result<(), DecodeError> {
        let (h, set) = (&f.h, &f.set);
        let mut xb = BitReader::new(f.data);
        xb.seek(pos + 32 + 10 + 4);
        let xset = SetHeader::parse(&mut xb, h.cpf)?;
        if xset.nch != 1 {
            return Err(DecodeError::Unsupported("XCh with more than one channel"));
        }
        // History for the XCh slot follows the core's continuity rule.
        nsubs_all[set.nch] = xset.nsubs[0];
        if !self.prev_xch {
            // The Cs synthesis history starts fresh when XCh (re)appears.
            self.qstate[set.nch].reset();
            if let Some(q) = self.q64state.get_mut(set.nch) {
                q.reset();
            }
        }
        let xkeep = f.keep && self.prev_xch;
        self.sub.start_frame(set.nch..set.nch + 1, h.blocks, xkeep);
        let mut xt0 = 0;
        for &nssc in &f.sf_nssc[..f.nsubfs] {
            let p = SubframeParams {
                nssc,
                dynf: false,
                lff: 0,
                ..f.params
            };
            subframe::decode_subframe(
                &mut xb,
                &xset,
                &mut self.side,
                &p,
                &mut self.sub,
                set.nch,
                xt0,
                None,
                nsubs_all,
            )?;
            xt0 += nssc * 8;
        }
        Ok(())
    }

    /// X96 extension (clause 6.2.4): the residual of a 64-band synthesis at twice the core
    /// rate. Returns whether the frame carries one.
    #[inline(always)]
    fn decode_x96(&mut self, f: &CoreFrame<'_>, pos: usize) -> Result<bool, DecodeError> {
        let (h, set) = (&f.h, &f.set);
        let mut xb = BitReader::new(f.data);
        xb.seek(pos + 32);
        let Some(xh) = x96::X96Header::parse(&mut xb, set.nch, h.cpf)? else {
            return Ok(false);
        };
        let buf = self.x96buf.get_or_insert_with(|| SubBuf::new(5, 64));
        buf.start_frame(0..set.nch, h.blocks, f.keep && self.prev_x96);
        let mut xt0 = 0;
        for &nssc in &f.sf_nssc[..f.nsubfs] {
            x96::decode_subframe(
                &mut xb,
                &xh,
                &mut self.x96side,
                nssc,
                h.aspf,
                h.cpf,
                h.rate == 0x1F,
                buf,
                xt0,
            )?;
            xt0 += nssc * 8;
        }
        Ok(true)
    }
}

/// Optional information (Table 5-30). It carries nothing the output depends on, and
/// FFmpeg treats malformed auxiliary data as non-fatal; it only bounds the search for
/// the extensions that follow. Returns the bit position where the core data ends.
#[inline(always)]
fn optional_info(br: &mut BitReader<'_>, f: &CoreFrame<'_>) -> usize {
    let h = &f.h;
    if h.timef {
        br.skip(32);
    }
    if h.auxf {
        let auxct = br.read(6) as usize;
        br.align(32);
        br.skip(auxct * 8);
    }
    if h.cpf && h.dynf {
        br.skip(16);
    }
    if br.overrun() {
        f.data.len() * 8
    } else {
        br.position()
    }
}

/// Core audio channel arrangements supported (AMODE 0..=9, Table 5-4): coded channel
/// order mapped to speakers.
fn amode_speakers(amode: u8) -> Option<&'static [Speaker]> {
    use Speaker::*;
    Some(match amode {
        0 => &[FrontCenter],
        1..=4 => &[FrontLeft, FrontRight],
        5 => &[FrontCenter, FrontLeft, FrontRight],
        6 => &[FrontLeft, FrontRight, BackCenter],
        7 => &[FrontCenter, FrontLeft, FrontRight, BackCenter],
        8 => &[FrontLeft, FrontRight, SideLeft, SideRight],
        9 => &[FrontCenter, FrontLeft, FrontRight, SideLeft, SideRight],
        _ => return None,
    })
}

/// Locate the XCh sync (Table 6-17): 32-bit aligned, at distance XChFSIZE+1 (or, for
/// legacy streams, XChFSIZE) bytes from the end of the frame.
fn find_xch(frame: &[u8], from_bit: usize) -> Option<usize> {
    let end = frame.len() * 8;
    let mut p = from_bit.div_ceil(32) * 32;
    while p + 64 <= end {
        let byte = p / 8;
        if frame[byte..byte + 4] == [0x5A, 0x5A, 0x5A, 0x5A] {
            let mut b = BitReader::new(frame);
            b.seek(p + 32);
            let fs = b.read(10) as usize;
            let dist = frame.len() - byte;
            if dist == fs + 1 || dist == fs {
                return Some(p);
            }
        }
        p += 32;
    }
    None
}

/// First 32-bit aligned occurrence of `sync` at or after `from_bit`.
fn find_sync32(frame: &[u8], from_bit: usize, sync: u32) -> Option<usize> {
    let mut byte = from_bit.div_ceil(32) * 4;
    while byte + 4 <= frame.len() {
        if u32::from_be_bytes([
            frame[byte],
            frame[byte + 1],
            frame[byte + 2],
            frame[byte + 3],
        ]) == sync
        {
            return Some(byte * 8);
        }
        byte += 4;
    }
    None
}
