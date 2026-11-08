# Third-party notices

The constant tables of this crate are taken from RFC 6716, "Definition of the Opus
Audio Codec" (IETF, September 2012), and its reference implementation, which the RFC
contains (Appendix A) under the licence below. The decoding code itself is written from
the RFC's description.

Taken from the RFC (CELT, `src/celt/tables.rs` and `src/celt/bands.rs`): the band edges
(`eband5ms`), mean band energies (`eMeans`), coarse energy prediction coefficients and
Laplace models (`pred_coef`, `beta_coef`, `beta_intra`, `e_prob_model`), the static
allocation vectors (`band_allocation`), `logN400`, the pulse cache (`cache_index50`,
`cache_bits50`, `cache_caps50`), the 120-sample window (`window120`), `LOG2_FRAC_TABLE`,
`tf_select_table`, the inverse CDFs of the frame header (`trim_icdf`, `spread_icdf`,
`tapset_icdf`, `small_energy_icdf`), the post-filter taps, the Hadamard order
(`ordery_table`), the collapse-mask interleaving tables and `exp2_table8`. The SILK
layer's tables (`src/silk`) come from the same source.

```
Copyright 1994-2011 IETF Trust, Xiph.Org, Skype Limited, Octasic,
                    Jean-Marc Valin, Timothy B. Terriberry,
                    CSIRO, Gregory Maxwell, Mark Borgerding,
                    Erik de Castro Lopo. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of Internet Society, IETF or IETF Trust, nor the
names of specific contributors, may be used to endorse or promote
products derived from this software without specific prior written
permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER
OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## SILK tables

`src/silk/tables.rs` holds the SILK tables of RFC 6716 section 4.2 (Tables 4 to 53): the
PDFs, stored as the inverse CDFs of the reference implementation's `silk/tables_*.c`,
the stereo weights, the LSF codebooks with their stage-2 selections, prediction weights
and minimum spacings, the cosine table, the pitch contour and LTP codebooks, the shell
coder split PDFs, the sign PDFs and the quantization offsets. They are under the licence
above. Every PDF and codebook was checked against the RFC's printed tables. The
interpolation filters in `src/silk/resampler.rs` are computed from their formula and
are not taken from any source.
