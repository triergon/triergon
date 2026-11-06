//! Development aid: rewrite an ADTS stream without its SBR fill elements (core-only
//! reference generation for the FFmpeg oracle). Single raw_data_block, no CRC only.
use triergon_codec_aac::{AudioFrame, Decoder, adts};

fn get(b: &[u8], pos: usize) -> u8 {
    (b[pos >> 3] >> (7 - (pos & 7))) & 1
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let mut out = Vec::new();
    let mut dec = Decoder::new();
    let mut f = AudioFrame::default();
    let mut pos = 0;
    while let Some((off, h)) = adts::find_frame(&data, pos) {
        let end = (off + h.frame_length).min(data.len());
        pos = end;
        let au = &data[off..end];
        if dec.decode(au, &mut f).is_err() || !h.protection_absent || h.raw_data_blocks != 1 {
            continue;
        }
        let payload = &au[7..];
        let nbits = payload.len() * 8;
        let ranges = dec.sbr_fill_ranges().to_vec();
        let mut bits: Vec<u8> = Vec::with_capacity(nbits);
        let mut p = 0;
        for &(s, e) in &ranges {
            while p < s {
                bits.push(get(payload, p));
                p += 1;
            }
            p = e;
        }
        while p < nbits {
            bits.push(get(payload, p));
            p += 1;
        }
        // drop trailing alignment of the old block: find END by re-alignment (the END id and
        // byte alignment are copied as-is; removed fills keep their byte-multiple length so
        // only the alignment padding may need trimming)
        while !bits.len().is_multiple_of(8) {
            bits.push(0);
        }
        let mut pl = Vec::new();
        for c in bits.chunks(8) {
            pl.push(c.iter().fold(0u8, |acc, &b| (acc << 1) | b));
        }
        let len = pl.len() + 7;
        let mut hd = au[..7].to_vec();
        hd[3] = (hd[3] & 0xfc) | ((len >> 11) as u8 & 3);
        hd[4] = (len >> 3) as u8;
        hd[5] = (hd[5] & 0x1f) | (((len & 7) as u8) << 5);
        out.extend_from_slice(&hd);
        out.extend_from_slice(&pl);
    }
    std::fs::write(&a[2], out).unwrap();
}
