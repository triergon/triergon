//! Test tooling: print per-syncframe bit positions (JSON lines) used by the bitstream
//! patching scripts in harness/codecs/ac3/ (block-switch / dither experiments).
//!
//! usage: trace <in>

use triergon_codec_ac3::{AudioFrame, Decoder, access_unit_len};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read");
    let mut dec = Decoder::new();
    dec.set_trace(true);
    let mut frame = AudioFrame::default();
    let mut pos = 0;
    let mut au = 0;
    while pos + 8 <= data.len() {
        let Ok(len) = access_unit_len(&data[pos..]) else {
            pos += 1;
            continue;
        };
        if pos + len > data.len() {
            break;
        }
        frame.clear();
        let r = dec.decode(&data[pos..pos + len], &mut frame);
        for t in &dec.trace().frames {
            println!(
                "{{\"au\":{au},\"offset\":{},\"frame_bytes\":{},\"eac3\":{},\"nfchans\":{},\"num_blocks\":{},\"dithflag_bits\":{:?},\"insert_bits\":{:?},\"dithflage_bit\":{},\"audio_end_bit\":{},\"ecplinu_bits\":{:?},\"transproce_bit\":{},\"transproc_data_bit\":{},\"dbaflde_bit\":{},\"blocks\":[{}],\"ok\":{}}}",
                pos + t.offset,
                t.frame_bytes,
                t.eac3,
                t.nfchans,
                t.num_blocks,
                t.dithflag_bits,
                t.dithflag_insert_bits,
                t.dithflage_bit.map_or(-1, |b| b as i64),
                t.audio_end_bit,
                t.ecplinu_bits,
                t.transproce_bit.map_or(-1, |b| b as i64),
                t.transproc_data_bit.map_or(-1, |b| b as i64),
                t.dbaflde_bit.map_or(-1, |b| b as i64),
                t.blocks
                    .iter()
                    .map(|b| format!(
                        "{{\"blk\":{},\"cplstre\":{},\"phsflginu_bit\":{},\"phsflg_bit\":{},\"ncplbnd\":{},\"cplcoe_any\":{},\"dba_bit\":{}}}",
                        b.blk,
                        b.cplstre,
                        b.phsflginu_bit.map_or(-1, |v| v as i64),
                        b.phsflg_bit,
                        b.ncplbnd,
                        b.cplcoe_any,
                        b.dba_bit
                    ))
                    .collect::<Vec<_>>()
                    .join(","),
                r.is_ok()
            );
        }
        pos += len;
        au += 1;
    }
}
