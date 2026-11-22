//! Parametric Stereo input used by the table-recovery checks.

use std::io::{BufWriter, Write};

use crate::{AudioFrame, Decoder, adts};

#[test]
#[ignore = "writes the QMF input of an explicitly requested ADTS stream"]
fn write_ps_input() {
    let input = std::env::var_os("AAC_PS_INPUT").expect("input path");
    let output = std::env::var_os("AAC_PS_OUTPUT").expect("output path");
    let data = std::fs::read(input).expect("read ADTS stream");
    let mut out = BufWriter::new(std::fs::File::create(output).expect("open QMF output"));
    let mut decoder = Decoder::new();
    let mut frame = AudioFrame::default();
    let mut pos = 0;
    let empty = vec![0f32; 38 * 64 * 2];
    while let Some((off, header)) = adts::find_frame(&data, pos) {
        let end = (off + header.frame_length).min(data.len());
        pos = end;
        let input = if decoder.decode(&data[off..end], &mut frame).is_ok() {
            decoder
                .core
                .elements
                .iter()
                .filter_map(|e| e.sbr.as_ref())
                .map(|sbr| sbr.ps_input.as_slice())
                .find(|slots| !slots.is_empty())
                .unwrap_or(&empty)
        } else {
            &empty
        };
        for v in input {
            out.write_all(&v.to_le_bytes()).expect("write QMF slots");
        }
    }
    out.flush().expect("flush QMF output");
}
