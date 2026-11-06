//! Python's `random.Random`, for the draws the corpus relies on: the seeded byte
//! stream of `randrange(256)`.
//!
//! This is tooling only and is never linked into a library crate.
//!
//! Derived work. The generator is MT19937 with its reference initialisation
//! (`init_genrand`, `init_by_array`) by Makoto Matsumoto and Takuji Nishimura.
//! Seeding from an integer (its little-endian 32-bit words as the key) and
//! bounded draws by rejection from `getrandbits` follow CPython's `random`
//! module. Changes from CPython: a Rust rendering of only these operations.
//!
//! Copyright (c) 2001 Python Software Foundation; All Rights Reserved. Used
//! under the Python Software Foundation License Version 2 (see
//! `xtask/THIRD-PARTY.md`).
//!
//! The Mersenne Twister notice follows.

// Copyright (C) 1997 - 2002, Makoto Matsumoto and Takuji Nishimura,
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
//
// 1. Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright
// notice, this list of conditions and the following disclaimer in the
// documentation and/or other materials provided with the distribution.
//
// 3. The names of its contributors may not be used to endorse or promote
// products derived from this software without specific prior written
// permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE COPYRIGHT OWNER
// OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

pub struct PyRandom {
    state: [u32; 624],
    index: usize,
}

impl PyRandom {
    pub fn new(seed: u32) -> Self {
        let mut mt = [0u32; 624];
        mt[0] = 19_650_218;
        for i in 1..624 {
            mt[i] = 1_812_433_253u32
                .wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        let key = [seed];
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..624.max(key.len()) {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= 624 {
                mt[0] = mt[623];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..623 {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1_566_083_941))
                .wrapping_sub(i as u32);
            i += 1;
            if i >= 624 {
                mt[0] = mt[623];
                i = 1;
            }
        }
        mt[0] = 0x8000_0000;
        PyRandom {
            state: mt,
            index: 624,
        }
    }

    fn next_u32(&mut self) -> u32 {
        if self.index >= 624 {
            for k in 0..624 {
                let y = (self.state[k] & 0x8000_0000) | (self.state[(k + 1) % 624] & 0x7fff_ffff);
                let mag = if y & 1 == 1 { 0x9908_b0df } else { 0 };
                self.state[k] = self.state[(k + 397) % 624] ^ (y >> 1) ^ mag;
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    /// `randrange(256)`.
    pub fn byte(&mut self) -> u8 {
        loop {
            let r = self.next_u32() >> (32 - 9);
            if r < 256 {
                return r as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_python_random() {
        // python3 -c "import random; r=random.Random(1951); print(bytes(r.randrange(256) for _ in range(8)).hex())"
        let mut r = PyRandom::new(1951);
        let bytes: Vec<u8> = (0..8).map(|_| r.byte()).collect();
        assert_eq!(
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            PYTHON_1951
        );
    }

    const PYTHON_1951: &str = "47f4b2da9c2f343f";
}
