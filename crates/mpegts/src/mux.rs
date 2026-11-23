//! Single-program MPEG-TS writer (ITU-T H.222.0 | ISO/IEC 13818-1) for one
//! video elementary stream, plus a DVB SDT (ETSI EN 300 468).
//!
//! Every picture is one PES whose first TS packet carries a PCR equal to DTS·300
//! (and the random-access indicator on key pictures); SDT/PAT/PMT precede a PES
//! when their period elapsed in PCR time, PAT/PMT also before a key picture
//! following a non-key picture.

use crate::pes::stream_id;
use crate::psi::{PsiCrc32, descriptor_tag, stream_type};
use crate::transport::pid;
use triergon_core::{Error, Result};

const PACKET: usize = 188;
const PCR_HZ_PER_90K: i64 = 300;
const PAT_PERIOD: i64 = 27_000_000 / 10;
const SDT_PERIOD: i64 = 27_000_000 / 2;

/// Identifiers, PIDs and service names of the single program a [`Muxer`] writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MuxConfig {
    /// The 16-bit transport stream ID written in the PAT and SDT.
    pub transport_stream_id: u16,
    /// The 16-bit original network ID written in the SDT.
    pub original_network_id: u16,
    /// The 16-bit program number of the single program; must be nonzero.
    pub program_number: u16,
    /// The 13-bit PID of the PMT, in 0x0020..0x1FFF.
    pub pmt_pid: u16,
    /// The 13-bit PID of the video stream, which also carries the PCR; in 0x0020..0x1FFF and
    /// distinct from `pmt_pid`.
    pub video_pid: u16,
    /// [`stream_type::MPEG1_VIDEO`] (the default), [`stream_type::MPEG2_VIDEO`] or
    /// [`stream_type::H264`]; the writer does not look inside the pictures.
    pub stream_type: u8,
    /// The SDT service provider name. Printable ASCII is written as it is; any other name
    /// without control characters as UTF-8 behind the 0x15 selector of EN 300 468 Annex A.
    pub provider_name: String,
    /// The SDT service name, encoded as `provider_name`. The two encoded names together
    /// take at most 252 bytes, the room one service descriptor has.
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
            stream_type: stream_type::MPEG1_VIDEO,
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
    /// A muxer for the program `cfg` describes, with its PAT, PMT and SDT serialised once.
    ///
    /// Fails with `Unsupported` when a name has a control character, the encoded names exceed
    /// the service descriptor, the program number is 0, a PID is outside 0x0020..0x1FFF or the
    /// two PIDs are equal, or the stream type is not 0x01, 0x02 or 0x1B.
    pub fn new(cfg: &MuxConfig) -> Result<Muxer> {
        let provider = service_text(&cfg.provider_name)?;
        let service = service_text(&cfg.service_name)?;
        // The service descriptor's 8-bit length covers the type and both length-prefixed names.
        if provider.len() + service.len() > 255 - 3 {
            return Err(Error::Unsupported("service descriptor length"));
        }
        if cfg.program_number == 0 {
            return Err(Error::Unsupported("zero program number"));
        }
        if !(0x20..pid::NULL).contains(&cfg.pmt_pid) || !(0x20..pid::NULL).contains(&cfg.video_pid)
        {
            return Err(Error::Unsupported("PMT/video PID must be usable13-bit PID"));
        }
        if cfg.pmt_pid == cfg.video_pid {
            return Err(Error::Unsupported("PMT/video PID collision"));
        }
        if !matches!(
            cfg.stream_type,
            stream_type::MPEG1_VIDEO | stream_type::MPEG2_VIDEO | stream_type::H264
        ) {
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
            let descriptor = 3 + provider.len() + service.len();
            let loop_len = 2 + descriptor;
            b.extend_from_slice(&((4u16 << 13) | loop_len as u16).to_be_bytes()); // running, not scrambled
            b.extend_from_slice(&[
                descriptor_tag::DVB_SERVICE,
                descriptor as u8,
                0x01,
                provider.len() as u8,
            ]);
            b.extend_from_slice(&provider);
            b.push(service.len() as u8);
            b.extend_from_slice(&service);
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
            self.write_section(pid::DVB_SDT, CC_SDT, &sdt, out);
            self.sdt = sdt;
        }
        let force = key && !self.previous_key;
        if force || self.last_pat.is_none_or(|last| pcr - last >= PAT_PERIOD) {
            self.last_pat = Some(self.last_pat.map_or(pcr, |l| l.max(pcr)));
            let (pat, pmt) = (std::mem::take(&mut self.pat), std::mem::take(&mut self.pmt));
            self.write_section(pid::PAT, CC_PAT, &pat, out);
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

    /// One section from a payload unit start with a zero pointer field, continued across
    /// further packets of `pid` when it does not fit; the last one is stuffed with 0xFF.
    fn write_section(&mut self, pid: u16, which: usize, section: &[u8], out: &mut Vec<u8>) {
        let cc = self.next_cc(which);
        let start = out.len();
        out.extend_from_slice(&[0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10 | cc, 0x00]);
        let (first, mut rest) = section.split_at(section.len().min(PACKET - 5));
        out.extend_from_slice(first);
        out.resize(start + PACKET, 0xFF);
        while !rest.is_empty() {
            let cc = self.next_cc(which);
            let start = out.len();
            out.extend_from_slice(&[0x47, (pid >> 8) as u8, pid as u8, 0x10 | cc]);
            let (part, tail) = rest.split_at(rest.len().min(PACKET - 4));
            out.extend_from_slice(part);
            out.resize(start + PACKET, 0xFF);
            rest = tail;
        }
    }

    fn write_pes(&mut self, es: &[u8], pts: i64, dts: i64, pcr: i64, key: bool, out: &mut Vec<u8>) {
        let mut header = [0u8; 19];
        header[..9].copy_from_slice(&[
            0x00,
            0x00,
            0x01,
            stream_id::VIDEO_FIRST,
            0x00,
            0x00,
            0x80,
            0xC0,
            10,
        ]);
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

/// `name` as EN 300 468 Annex A text: printable ASCII as it is (the default character
/// table agrees with ASCII there), any other text as UTF-8 behind the 0x15 selector.
/// Control characters, which the table assigns to presentation codes, are refused.
fn service_text(name: &str) -> Result<Vec<u8>> {
    if name.chars().any(char::is_control) {
        return Err(Error::Unsupported(
            "service name encoding: control character",
        ));
    }
    let mut text = Vec::new();
    if !name.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        text.push(0x15);
    }
    text.extend_from_slice(name.as_bytes());
    if text.len() > 255 {
        return Err(Error::Unsupported("service name length"));
    }
    Ok(text)
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
            stream_type: stream_type::MPEG2_VIDEO,
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
    fn avc_stream_type_is_written_to_the_pmt() {
        let m = Muxer::new(&MuxConfig {
            stream_type: stream_type::H264,
            ..MuxConfig::default()
        })
        .unwrap();
        assert_eq!(m.pmt[12], stream_type::H264);
        assert_eq!(PsiCrc32::of(&m.pmt), 0);
    }

    #[test]
    fn names_outside_ascii_are_utf8_behind_the_annex_a_selector() {
        let m = Muxer::new(&MuxConfig {
            service_name: "Caf\u{e9}".into(),
            ..MuxConfig::default()
        })
        .unwrap();
        let service = [6, 0x15, b'C', b'a', b'f', 0xc3, 0xa9];
        assert!(m.sdt.windows(service.len()).any(|w| w == service));
        assert_eq!(PsiCrc32::of(&m.sdt), 0);
        let refused = Muxer::new(&MuxConfig {
            provider_name: "Test\tLab".into(),
            ..MuxConfig::default()
        });
        assert_eq!(
            refused.unwrap_err(),
            Error::Unsupported("service name encoding: control character")
        );
    }

    #[test]
    fn an_sdt_longer_than_one_packet_continues_on_the_next() {
        let config = MuxConfig {
            provider_name: "P".repeat(80),
            service_name: "S".repeat(79),
            ..MuxConfig::default()
        };
        let mut m = Muxer::new(&config).unwrap();
        assert!(m.sdt.len() > PACKET - 5);
        let mut out = Vec::new();
        m.write_picture(&[0; 10], 3000, 0, true, &mut out);
        let sdt: Vec<&[u8]> = out
            .chunks(PACKET)
            .filter(|p| u16::from_be_bytes([p[1] & 0x1F, p[2]]) == pid::DVB_SDT)
            .collect();
        assert_eq!(sdt.len(), 2);
        assert_eq!((sdt[0][1] & 0x40, sdt[1][1] & 0x40), (0x40, 0));
        assert_eq!((sdt[0][3] & 0x0F, sdt[1][3] & 0x0F), (0, 1));
        let mut section = sdt[0][5..].to_vec();
        section.extend_from_slice(&sdt[1][4..]);
        section.truncate(m.sdt.len());
        assert_eq!(section, m.sdt);
        assert!(
            sdt[1][4 + m.sdt.len() - (PACKET - 5)..]
                .iter()
                .all(|&b| b == 0xFF)
        );
        assert!(
            Muxer::new(&MuxConfig {
                service_name: "S".repeat(173),
                ..config
            })
            .is_err()
        );
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
