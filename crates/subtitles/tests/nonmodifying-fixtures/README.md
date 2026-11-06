# DVB non-modifying colour fixtures

Twelve authored objects for `tests/nonmodifying.rs`, covering 2/4/8-bit depth,
literal and run encoding, and the non-modifying colour flag off and on. Each 5x2
region is filled with CLUT entry 3 and coded `11112` per row: flag 0 writes
`11112`, flag 1 leaves `33332`. Each case has the `.dvb` packet, `.ods`, `.cds`,
`.palette`, and the expected `.indices` and `.rgba`.

Reference: ETSI EN 300 743 V1.6.1 §7.2.5 and Tables 22, 24 and 26. The
FFmpeg-compatible view differs for flag 1; see `tests/native_nonmodifying.rs`.
