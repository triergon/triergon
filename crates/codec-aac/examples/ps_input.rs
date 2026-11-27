//! Parametric Stereo input used by the table-recovery checks.

use std::io::{BufWriter, Write};

use triergon_codec_aac::compat::ffmpeg::extensions::Policy;
use triergon_codec_aac::{AudioFrame, Decoder, adts, reference_trace};

fn main() -> std::io::Result<()> {
    let input = std::env::var_os("AAC_PS_INPUT").expect("input path");
    let output = std::env::var_os("AAC_PS_OUTPUT").expect("output path");
    let data = std::fs::read(input)?;
    let mut out = BufWriter::new(std::fs::File::create(output)?);
    let mut decoder = Decoder::new().with_extension_policy(Policy::Reference);
    let mut frame = AudioFrame::default();
    let mut pos = 0;
    let empty = vec![0f32; 38 * 64 * 2];
    while let Some((off, header)) = adts::find_frame(&data, pos) {
        let end = (off + header.frame_length).min(data.len());
        pos = end;
        let input = if decoder.decode(&data[off..end], &mut frame).is_ok() {
            reference_trace::ps_input(&decoder).unwrap_or(&empty)
        } else {
            &empty
        };
        for v in input {
            out.write_all(&v.to_le_bytes())?;
        }
    }
    out.flush()
}
