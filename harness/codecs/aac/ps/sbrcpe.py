"""Crafted HE-AAC ADTS streams with a coupled SBR channel pair, for black-box probes of a
decoder's SBR balance coding.

Every frame is one raw data block at a 22.05 kHz core (44.1 kHz output): a CPE (no common
window) whose two long-window spectra hold seeded random codebook-1 quads, followed by a
fill element with SBR data for the pair: a header in every frame (amp_res 1, start_freq 15,
stop_freq 14: kx = 32, five low-resolution envelope bands, two noise bands), bs_coupling 1,
a FIXFIX grid with two envelopes at low resolution (so the 3.0 dB books apply), frequency
(df) coding throughout, and the caller's bits for the first channel's envelopes and noise
floors and for the second (balance) channel's. Bits after the noise data are the caller's
too (add_harmonic flags, extended data).

Only the AAC and SBR syntax this repository's decoder implements is used; the AAC books
are read from crates/codec-aac/src/huffman_tables.rs.
"""
import psgen

N_ENV_BANDS = 5   # low-resolution envelope bands for this header at 44.1 kHz
N_NOISE = 2       # noise bands


class Gen:
    def __init__(self, rng, global_gain=164, max_sfb=47):
        self.core = psgen.Gen(rng, global_gain=global_gain, max_sfb=max_sfb)

    def ics(self, w):
        g = self.core
        w.put(g.gg, 8)
        w.put(0, 1); w.put(0, 2); w.put(0, 1)
        w.put(g.max_sfb, 6); w.put(0, 1)
        w.put(1, 4)
        left = g.max_sfb
        while left >= 31:
            w.put(31, 5); left -= 31
        w.put(left, 5)
        for _ in range(g.max_sfb):
            w.code(g.sf, 60)
        w.put(0, 1); w.put(0, 1); w.put(0, 1)
        for b in range(g.max_sfb):
            for _ in range((psgen.SWB_LONG_24[b + 1] - psgen.SWB_LONG_24[b]) // 4):
                w.code(g.sp1, int(g.rng.choice(g.quad_syms)))

    @staticmethod
    def sbr(ch0, ch1, tail):
        """SBR payload bits after the extension type. ch0 / ch1: the envelope-and-noise
        bits of the level channel and the balance channel (everything from the first
        envelope's start value to the last noise floor). tail: the bits that follow
        (add_harmonic flags, bs_extended_data, ...)."""
        w = psgen.BW()
        w.put(1, 1)                                         # bs_header_flag
        w.put(1, 1); w.put(15, 4); w.put(14, 4); w.put(0, 3)  # amp_res, start, stop, xover
        w.put(0, 2); w.put(0, 1); w.put(0, 1)               # reserved, no extra fields
        w.put(0, 1)                                         # bs_data_extra
        w.put(1, 1)                                         # bs_coupling
        w.put(0, 2); w.put(1, 2); w.put(0, 1)               # FIXFIX, two envelopes, low res
        w.put(0, 4)                                         # dtdf, channel 0: all df
        w.put(0, 4)                                         # dtdf, channel 1: all df
        w.put(0, 2 * N_NOISE)                               # invf
        w.putbits(ch0)
        w.putbits(ch1)
        w.putbits(tail)
        return w.bits

    def frame(self, ch0, ch1, tail, fil_bytes=None):
        w = psgen.BW()
        w.put(1, 3); w.put(0, 4); w.put(0, 1)               # CPE, tag 0, no common window
        self.ics(w)
        self.ics(w)
        s = self.sbr(ch0, ch1, tail)
        nbits = 4 + len(s)
        cnt = (nbits + 7) // 8 if fil_bytes is None else fil_bytes
        w.put(6, 3)
        if cnt >= 15:
            w.put(15, 4); w.put(cnt - 14, 8)
        else:
            w.put(cnt, 4)
        w.put(13, 4)
        w.bits += s
        w.bits += [0] * max(0, cnt * 8 - nbits)
        w.put(7, 3)
        return adts_stereo(psgen.adts(w.bytes()))


def stream(path, frames):
    psgen.write_stream(path, frames)


def adts_stereo(frame_bytes):
    """psgen.adts writes channel configuration 1; patch it to 2."""
    b = bytearray(frame_bytes)
    # channel_configuration: 3 bits spanning byte 2 bit 0 and byte 3 bits 7..6
    b[2] = (b[2] & 0xfe) | 0
    b[3] = (b[3] & 0x3f) | (2 << 6)
    return bytes(b)
