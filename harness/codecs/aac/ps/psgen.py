"""Crafted HE-AAC v2 ADTS streams for black-box probes of a decoder's Parametric Stereo tool.

Every frame is one raw data block at a 22.05 kHz core (44.1 kHz output): a mono SCE whose
long-window spectrum holds seeded random codebook-1 quads in all 47 scalefactor bands,
followed by a fill element with SBR data (a header in every frame; start_freq 15,
stop_freq 14, so kx = 32 and M = 32; one FIXFIX envelope at low frequency resolution with
a flat level; the lowest noise floor). The SBR extended data carries one extension with
id 2, whose payload bits the caller chooses; any other ids can be added.

Only the AAC and SBR syntax this repository's decoder implements is used; the codebooks are
read from crates/codec-aac/src/huffman_tables.rs.
"""
import os
import re

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..', '..', '..'))
TABLES = os.path.join(ROOT, 'crates', 'codec-aac', 'src', 'huffman_tables.rs')

SWB_LONG_24 = [0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 76, 84, 92, 100, 108,
               116, 124, 136, 148, 160, 172, 188, 204, 220, 240, 260, 284, 308, 336, 364, 396,
               432, 468, 508, 552, 600, 652, 704, 768, 832, 896, 960, 1024]


def load_book(name):
    text = open(TABLES).read()
    m = re.search(r'static %s: \[\(u32, u8\); (\d+)\] = \[(.*?)\];' % name, text, re.S)
    pairs = re.findall(r'\((0x[0-9a-f]+|\d+), (\d+)\)', m.group(2))
    book = [(int(c, 0), int(n)) for c, n in pairs]
    assert len(book) == int(m.group(1))
    return book


class BW:
    def __init__(self):
        self.bits = []

    def put(self, v, n):
        for i in range(n - 1, -1, -1):
            self.bits.append((v >> i) & 1)

    def putbits(self, s):
        self.bits.extend(int(c) for c in s)

    def code(self, book, sym):
        c, n = book[sym]
        self.put(c, n)

    def bytes(self):
        b = self.bits + [0] * ((8 - len(self.bits) % 8) % 8)
        return bytes(int(''.join(map(str, b[i:i + 8])), 2) for i in range(0, len(b), 8))


def adts(payload, sf_index=7):
    n = len(payload) + 7
    h = BW()
    h.put(0xfff, 12); h.put(0, 1); h.put(0, 2); h.put(1, 1)
    h.put(1, 2); h.put(sf_index, 4); h.put(0, 1); h.put(1, 3)
    h.put(0, 4); h.put(n, 13); h.put(0x7ff, 11); h.put(0, 2)
    return h.bytes() + payload


class Gen:
    """Stream builder. `rng` is a numpy Generator for the core spectrum."""

    def __init__(self, rng, global_gain=164, env_level=20, max_sfb=47, quad_syms=None,
                 bins=None):
        self.rng = rng
        self.gg = global_gain
        self.env_level = env_level
        self.max_sfb = max_sfb
        self.sp1 = load_book('SPECTRUM_1')
        self.sf = load_book('SCALEFACTOR')
        self.f_env15 = load_book('SBR_F_ENV15')
        self.f_env30 = load_book('SBR_F_ENV30')
        self.quad_syms = quad_syms if quad_syms is not None else list(range(81))
        # optional set of spectral lines (0..1023) allowed to be nonzero
        self.bins = bins

    def sce(self, w, silent=False):
        w.put(0, 3); w.put(0, 4)           # SCE, tag 0
        w.put(self.gg, 8)                   # global_gain
        w.put(0, 1); w.put(0, 2); w.put(0, 1)  # reserved, ONLY_LONG, sine window
        nsfb = 0 if silent else self.max_sfb
        w.put(nsfb, 6); w.put(0, 1)         # max_sfb, no prediction
        if nsfb:
            w.put(1, 4)                     # section: codebook 1 for every band
            left = nsfb
            while left >= 31:
                w.put(31, 5); left -= 31
            w.put(left, 5)
            for _ in range(nsfb):
                w.code(self.sf, 60)         # scalefactor delta 0
        w.put(0, 1); w.put(0, 1); w.put(0, 1)  # pulse, TNS, gain control
        for b in range(nsfb):
            for q in range(SWB_LONG_24[b], SWB_LONG_24[b + 1], 4):
                sym = int(self.rng.choice(self.quad_syms))
                if self.bins is not None and not any(q + i in self.bins for i in range(4)):
                    sym = 40                    # (0, 0, 0, 0)
                w.code(self.sp1, sym)

    def sbr(self, extensions, declared=None, tail=''):
        """SBR payload bits (after the 4-bit extension type). `extensions` is a list of
        (id, bitstring) pairs placed in bs_extended_data; each payload is padded with zero
        bits to the whole bytes the size field counts. With `declared`, the size field
        says `declared` bytes whatever the payload's length (the payload is still written
        whole), and `tail` bits follow the padded extension data."""
        w = BW()
        w.put(1, 1)                         # bs_header_flag
        w.put(1, 1); w.put(15, 4); w.put(14, 4); w.put(0, 3)  # amp_res, start, stop, xover
        w.put(0, 2); w.put(0, 1); w.put(0, 1)  # reserved, no extra header fields
        w.put(0, 1)                         # bs_data_extra
        w.put(0, 2); w.put(0, 2); w.put(0, 1)  # FIXFIX, one envelope, low resolution
        w.put(0, 1); w.put(0, 1)            # df coding of envelope and noise
        w.put(0, 2); w.put(0, 2)            # invf for both noise bands
        w.put(self.env_level, 7)            # envelope: start value
        for _ in range(4):
            w.code(self.f_env15, 60)        # flat
        w.put(30, 5)                        # noise floor start value (lowest level)
        w.code(self.f_env30, 31)            # flat
        w.put(0, 1)                         # bs_add_harmonic_flag
        if extensions:
            body = BW()
            for ext_id, bits in extensions:
                body.put(ext_id, 2)
                body.putbits(bits)
            nbits = len(body.bits)
            cnt = (nbits + 7) // 8
            body.bits += [0] * (cnt * 8 - nbits)
            if declared is not None:
                cnt = declared
            w.put(1, 1)
            if cnt >= 15:
                w.put(15, 4); w.put(cnt - 15, 8)
            else:
                w.put(cnt, 4)
            w.bits += body.bits
            w.putbits(tail)
        else:
            w.put(0, 1)
        return w.bits

    def frame(self, extensions, silent=False, sbr=True, declared=None, tail=''):
        w = BW()
        self.sce(w, silent)
        if sbr:
            s = self.sbr(extensions, declared, tail)
            nbits = 4 + len(s)
            cnt = (nbits + 7) // 8
            w.put(6, 3)                     # FIL
            if cnt >= 15:
                w.put(15, 4); w.put(cnt - 14, 8)
            else:
                w.put(cnt, 4)
            w.put(13, 4)                    # EXT_SBR_DATA
            w.bits += s
            w.bits += [0] * (cnt * 8 - nbits)
        w.put(7, 3)                         # END
        return adts(w.bytes())


def write_stream(path, frames):
    with open(path, 'wb') as f:
        for fr in frames:
            f.write(fr)
