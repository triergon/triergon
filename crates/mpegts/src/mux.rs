//! Single-program MPEG-TS writer (ITU-T H.222.0 | ISO/IEC 13818-1) for one
//! video elementary stream, plus a DVB SDT (ETSI EN 300 468).
//!
//! Cadence and layout follow the `mux` contract in `harness/capture/contract.json`:
//! every picture is one PES whose first TS packet carries a PCR equal to DTS·300
//! (and the random-access indicator on key pictures); SDT/PAT/PMT precede a PES
//! when their period elapsed in PCR time, PAT/PMT also before a key picture
//! following a non-key picture.

use crate::psi::PsiCrc32;
use triergon_core::{Error, Result};

const PACKET: usize = 188;
const PAT_PID: u16 = 0x0000;
const SDT_PID: u16 = 0x0011;
const PCR_HZ_PER_90K: i64 = 300;
const PAT_PERIOD: i64 = 27_000_000 / 10;
const SDT_PERIOD: i64 = 27_000_000 / 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MuxConfig {
    pub transport_stream_id: u16,
    pub original_network_id: u16,
    pub program_number: u16,
    pub pmt_pid: u16,
    pub video_pid: u16,
    /// 0x01 (ISO/IEC 11172-2 video, normative default) or 0x02 (FFmpeg's choice).
    pub stream_type: u8,
    pub provider_name: String,
    pub service_name: String,
}

impl Default for MuxConfig {
    fn default() -> MuxConfig {
        MuxConfig {
            transport_stream_id: 1,
            original_network_id: 0xFF01,
            program_number: 1,
            pmt_pid: 0x1000,
            video_pid: 0x0100,
            stream_type: 0x01,
            provider_name: "Triergon".into(),
            service_name: "Service01".into(),
        }
    }
}

/// Muxer state. Tables are serialised once; only continuity counters change.
#[derive(Debug)]
pub struct Muxer {
    video_pid: u16,
    pat: Vec<u8>,
    pmt: Vec<u8>,
    sdt: Vec<u8>,
    pmt_pid: u16,
    cc: [u8; 4],
    last_pat: Option<i64>,
    last_sdt: Option<i64>,
    previous_key: bool,
}

const CC_PAT: usize = 0;
const CC_PMT: usize = 1;
const CC_SDT: usize = 2;
const CC_VIDEO: usize = 3;

impl Muxer {
    pub fn new(cfg: &MuxConfig) -> Result<Muxer> {
        // One complete SDT is supported per packet: 4-byte TS header, pointer,
        // 25 bytes of section/descriptor/CRC overhead, then the two names.
        for name in [&cfg.provider_name, &cfg.service_name] {
            if name.len() > 255 {
                return Err(Error::Unsupported("service name length"));
            }
            if !name.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
                return Err(Error::Unsupported(
                    "service name encoding: printable ASCII required",
                ));
            }
        }
        let names = cfg
            .provider_name
            .len()
            .checked_add(cfg.service_name.len())
            .ok_or(Error::Unsupported("service descriptor length"))?;
        if names > PACKET - 30 {
            return Err(Error::Unsupported("SDT exceeds one packet"));
        }
        if cfg.program_number == 0 {
            return Err(Error::Unsupported("zero program number"));
        }
        if !(0x20..0x1fff).contains(&cfg.pmt_pid) || !(0x20..0x1fff).contains(&cfg.video_pid) {
            return Err(Error::Unsupported("PMT/video PID must be usable13-bit PID"));
        }
        if cfg.pmt_pid == cfg.video_pid {
            return Err(Error::Unsupported("PMT/video PID collision"));
        }
        if !matches!(cfg.stream_type, 0x01 | 0x02) {
            return Err(Error::Unsupported("unselected stream type"));
        }

        let mut pat = Vec::new();
        section(&mut pat, 0x00, cfg.transport_stream_id, |b| {
            b.extend_from_slice(&cfg.program_number.to_be_bytes());
            b.extend_from_slice(&(0xE000 | cfg.pmt_pid).to_be_bytes());
        });
        let mut pmt = Vec::new();
        section(&mut pmt, 0x02, cfg.program_number, |b| {
            b.extend_from_slice(&(0xE000 | cfg.video_pid).to_be_bytes()); // PCR_PID
            b.extend_from_slice(&0xF000u16.to_be_bytes()); // program_info_length 0
            b.push(cfg.stream_type);
            b.extend_from_slice(&(0xE000 | cfg.video_pid).to_be_bytes());
            b.extend_from_slice(&0xF000u16.to_be_bytes()); // ES_info_length 0
        });
        let mut sdt = Vec::new();
        section(&mut sdt, 0x42, cfg.transport_stream_id, |b| {
            b.extend_from_slice(&cfg.original_network_id.to_be_bytes());
            b.push(0xFF); // reserved_future_use
            b.extend_from_slice(&cfg.program_number.to_be_bytes()); // service_id
            b.push(0xFC); // reserved, no EIT schedule / present-following
            let provider = cfg.provider_name.as_bytes();
            let service = cfg.service_name.as_bytes();
            let descriptor = 3 + provider.len() + service.len();
            let loop_len = 2 + descriptor;
            b.extend_from_slice(&((4u16 << 13) | loop_len as u16).to_be_bytes()); // running, not scrambled
            b.extend_from_slice(&[0x48, descriptor as u8, 0x01, provider.len() as u8]);
            b.extend_from_slice(provider);
            b.push(service.len() as u8);
            b.extend_from_slice(service);
        });
        Ok(Muxer {
            video_pid: cfg.video_pid,
            pat,
            pmt,
            sdt,
            pmt_pid: cfg.pmt_pid,
            cc: [0; 4],
            last_pat: None,
            last_sdt: None,
            previous_key: false,
        })
    }

    /// Append the TS packets for one picture to `out`. `pts`/`dts` are 90 kHz.
    pub fn write_picture(&mut self, es: &[u8], pts: i64, dts: i64, key: bool, out: &mut Vec<u8>) {
        let pcr = dts * PCR_HZ_PER_90K;
        if self.last_sdt.is_none_or(|last| pcr - last >= SDT_PERIOD) {
            self.last_sdt = Some(self.last_sdt.map_or(pcr, |l| l.max(pcr)));
            let sdt = std::mem::take(&mut self.sdt);
            self.write_section(SDT_PID, CC_SDT, &sdt, out);
            self.sdt = sdt;
        }
        let force = key && !self.previous_key;
        if force || self.last_pat.is_none_or(|last| pcr - last >= PAT_PERIOD) {
            self.last_pat = Some(self.last_pat.map_or(pcr, |l| l.max(pcr)));
            let (pat, pmt) = (std::mem::take(&mut self.pat), std::mem::take(&mut self.pmt));
            self.write_section(PAT_PID, CC_PAT, &pat, out);
            self.write_section(self.pmt_pid, CC_PMT, &pmt, out);
            (self.pat, self.pmt) = (pat, pmt);
        }
        self.write_pes(es, pts, dts, pcr, key, out);
        self.previous_key = key;
    }

    fn next_cc(&mut self, which: usize) -> u8 {
        let cc = self.cc[which];
        self.cc[which] = (cc + 1) & 0x0F;
        cc
    }

    fn write_section(&mut self, pid: u16, which: usize, section: &[u8], out: &mut Vec<u8>) {
        let cc = self.next_cc(which);
        let start = out.len();
        out.extend_from_slice(&[0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10 | cc, 0x00]);
        out.extend_from_slice(section);
        out.resize(start + PACKET, 0xFF);
    }

    fn write_pes(&mut self, es: &[u8], pts: i64, dts: i64, pcr: i64, key: bool, out: &mut Vec<u8>) {
        let mut header = [0u8; 19];
        header[..9].copy_from_slice(&[0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0xC0, 10]);
        timestamp(&mut header[9..14], 0b0011, pts);
        timestamp(&mut header[14..19], 0b0001, dts);
        let mut payload = es;
        let mut first = true;
        while first || !payload.is_empty() {
            let packet_start = out.len();
            let cc = self.next_cc(CC_VIDEO);
            let pusi = if first { 0x40 } else { 0x00 };
            out.extend_from_slice(&[
                0x47,
                pusi | (self.video_pid >> 8) as u8,
                self.video_pid as u8,
                0x10 | cc,
            ]);
            // Adaptation field content before stuffing: flags + PCR on the first packet.
            let mut af = [0u8; 7];
            let mut af_len = 0;
            if first {
                af[0] = if key { 0x50 } else { 0x10 };
                pcr_bits(&mut af[1..7], pcr);
                af_len = 7;
            }
            let pes_header: &[u8] = if first { &header } else { &[] };
            let overhead = 4 + if af_len > 0 { 1 + af_len } else { 0 } + pes_header.len();
            let room = PACKET - overhead;
            let take = room.min(payload.len());
            let stuffing = room - take;
            if af_len > 0 || stuffing > 0 {
                out[packet_start + 3] |= 0x20;
                if af_len > 0 {
                    out.push((af_len + stuffing) as u8);
                    out.extend_from_slice(&af[..af_len]);
                    out.resize(out.len() + stuffing, 0xFF);
                } else {
                    out.push((stuffing - 1) as u8);
                    if stuffing >= 2 {
                        out.push(0x00);
                        out.resize(out.len() + stuffing - 2, 0xFF);
                    }
                }
            }
            out.extend_from_slice(pes_header);
            out.extend_from_slice(&payload[..take]);
            payload = &payload[take..];
            first = false;
            debug_assert_eq!(out.len() - packet_start, PACKET);
        }
    }
}

/// Build a long-form PSI/SI section with version 0, current, section 0 of 0.
fn section(out: &mut Vec<u8>, table_id: u8, extension: u16, body: impl FnOnce(&mut Vec<u8>)) {
    out.push(table_id);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&extension.to_be_bytes());
    out.extend_from_slice(&[0xC1, 0x00, 0x00]);
    body(out);
    let length = out.len() - 3 + 4;
    // SDT uses reserved_future_use = 1 after the syntax indicator; PAT/PMT '0'.
    let high = if table_id == 0x42 { 0xF0 } else { 0xB0 };
    out[1] = high | (length >> 8) as u8;
    out[2] = length as u8;
    let crc = PsiCrc32::of(out);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn timestamp(out: &mut [u8], prefix: u8, t: i64) {
    let t = t as u64 & ((1 << 33) - 1);
    out[0] = prefix << 4 | ((t >> 29) as u8 & 0x0E) | 1;
    out[1] = (t >> 22) as u8;
    out[2] = ((t >> 14) as u8 & 0xFE) | 1;
    out[3] = (t >> 7) as u8;
    out[4] = ((t << 1) as u8 & 0xFE) | 1;
}

fn pcr_bits(out: &mut [u8], pcr: i64) {
    let base = (pcr / 300) as u64 & ((1 << 33) - 1);
    let ext = (pcr % 300) as u64;
    out[0] = (base >> 25) as u8;
    out[1] = (base >> 17) as u8;
    out[2] = (base >> 9) as u8;
    out[3] = (base >> 1) as u8;
    out[4] = ((base << 7) as u8 & 0x80) | 0x7E | (ext >> 8) as u8;
    out[5] = ext as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Section bytes (after the pointer field) observed from the FFmpeg 9.0.2
    /// reference:
    /// identical apart from the declared stream_type and names.
    #[test]
    fn tables_match_observed_layout() {
        let ffmpeg = MuxConfig {
            stream_type: 0x02,
            provider_name: "FFmpeg".into(),
            ..MuxConfig::default()
        };
        let m = Muxer::new(&ffmpeg).unwrap();
        assert_eq!(hex(&m.pat), "00b00d0001c100000001f0002ab104b2");
        assert_eq!(hex(&m.pmt), "02b0120001c10000e100f00002e100f0009e8b23d1");
        // The observation captured the SDT up to the first three CRC bytes.
        assert!(hex(&m.sdt).starts_with(
            "42f0250001c10000ff01ff0001fc80144812010646466d70656709536572766963653031777c43"
        ));
    }

    #[test]
    fn crc_check_value() {
        assert_eq!(PsiCrc32::of(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn pes_packetization() {
        let mut m = Muxer::new(&MuxConfig::default()).unwrap();
        let es: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let mut out = Vec::new();
        m.write_picture(&es, 3000, 0, true, &mut out);
        assert_eq!(out.len() % PACKET, 0);
        let packets: Vec<&[u8]> = out.chunks(PACKET).collect();
        // SDT, PAT, PMT, then the PES.
        let pids: Vec<u16> = packets
            .iter()
            .map(|p| u16::from_be_bytes([p[1] & 0x1F, p[2]]))
            .collect();
        assert_eq!(&pids[..3], &[0x11, 0x00, 0x1000]);
        let first = packets[3];
        assert_eq!(first[1] & 0x40, 0x40);
        assert_eq!(first[3] & 0x30, 0x30);
        assert_eq!(&first[4..6], &[7, 0x50]);
        assert_eq!(
            hex(&first[12..31]),
            "000001e0000080c00a31000117711100010001"
        );
        let mut payload = Vec::new();
        for (i, p) in packets[3..].iter().enumerate() {
            assert_eq!(p[3] & 0x0F, i as u8 & 0x0F);
            let start = if p[3] & 0x20 != 0 {
                5 + p[4] as usize
            } else {
                4
            };
            payload.extend_from_slice(&p[start..]);
        }
        assert_eq!(&payload[19..], &es[..]);
    }

    #[test]
    fn table_cadence() {
        let mut m = Muxer::new(&MuxConfig::default()).unwrap();
        let mut sdt_at = Vec::new();
        let mut pat_at = Vec::new();
        for k in 0..61i64 {
            let mut out = Vec::new();
            m.write_picture(&[0; 10], 3000 * (k + 1), 3000 * k, k % 30 == 0, &mut out);
            let pids: Vec<u16> = out
                .chunks(PACKET)
                .map(|p| u16::from_be_bytes([p[1] & 0x1F, p[2]]))
                .collect();
            if pids.contains(&0x11) {
                sdt_at.push(k);
            }
            if pids.contains(&0x00) {
                pat_at.push(k);
            }
        }
        assert_eq!(sdt_at, [0, 15, 30, 45, 60]);
        assert_eq!(pat_at, (0..61).step_by(3).collect::<Vec<_>>());
    }
}
