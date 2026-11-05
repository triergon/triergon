//! Development aid: rewrite an ADTS stream so that every SBR fill element keeps its
//! extension type but carries an all-zero payload (bs_header_flag = 0, no header ever).
//! A decoder then signals SBR implicitly (2x output rate) but never starts it, so the output
//! is the pure QMF analysis + synthesis of the core signal: a black-box probe of the QMF
//! banks. Single raw_data_block, no CRC only.
use triergon_codec_aac::{AudioFrame, Decoder, adts};

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
        let mut au = data[off..end].to_vec();
        if dec.decode(&au.clone(), &mut f).is_err()
            || !h.protection_absent
            || h.raw_data_blocks != 1
        {
            continue;
        }
        for &(s, e) in dec.debug_sbr_fil_ranges() {
            // FIL id (3) + count (4) [+ esc_count (8)] + extension_type (4)
            let get = |p: usize| (au[7 + (p >> 3)] >> (7 - (p & 7))) & 1;
            let count = (0..4).fold(0, |v, i| (v << 1) | get(s + 3 + i) as usize);
            let keep = 3 + 4 + if count == 15 { 8 } else { 0 } + 4;
            for p in s + keep..e {
                au[7 + (p >> 3)] &= !(0x80u8 >> (p & 7));
            }
        }
        out.extend_from_slice(&au);
    }
    std::fs::write(&a[2], out).unwrap();
}
