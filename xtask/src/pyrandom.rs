//! Python's `random.Random`, for the draws the corpus and harness rely on: seeded
//! byte streams, `random()`, `choices` and `shuffle`, each bit-identical to CPython.
//!
//! This is tooling only and is never linked into a library crate.
//!
//! Derived work. The generator is MT19937 with its reference initialisation
//! (`init_genrand`, `init_by_array`) by Makoto Matsumoto and Takuji Nishimura.
//! Seeding from an integer (its little-endian 32-bit words as the key), bounded
//! draws by rejection from `getrandbits`, the 53-bit `random()`, `choices` and
//! `shuffle` follow CPython's `random` module. Changes from CPython: a Rust
//! rendering of only these operations.
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
    pub fn new(seed: u64) -> Self {
        let mut mt = [0u32; 624];
        mt[0] = 19_650_218;
        for i in 1..624 {
            mt[i] = 1_812_433_253u32
                .wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        // Little-endian 32-bit words of the seed, without high zero words.
        let key: Vec<u32> = if seed >> 32 == 0 {
            vec![seed as u32]
        } else {
            vec![seed as u32, (seed >> 32) as u32]
        };
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

    /// `getrandbits(k)` for `1 <= k <= 32`.
    fn bits(&mut self, k: u32) -> u32 {
        self.next_u32() >> (32 - k)
    }

    /// `_randbelow(n)` for `n >= 1`.
    pub fn below(&mut self, n: u32) -> u32 {
        let k = 32 - n.leading_zeros();
        loop {
            let r = self.bits(k);
            if r < n {
                return r;
            }
        }
    }

    /// `randrange(256)`.
    pub fn byte(&mut self) -> u8 {
        self.below(256) as u8
    }

    /// `random()`: 53 random bits as a float in [0, 1).
    pub fn random(&mut self) -> f64 {
        let a = f64::from(self.next_u32() >> 5);
        let b = f64::from(self.next_u32() >> 6);
        (a * 67_108_864.0 + b) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// `choices(population, k=k)` without weights.
    pub fn choices<T: Copy>(&mut self, population: &[T], k: usize) -> Vec<T> {
        let n = population.len() as f64;
        (0..k)
            .map(|_| population[(self.random() * n).floor() as usize])
            .collect()
    }

    /// `shuffle(items)` in place.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u32 + 1) as usize;
            items.swap(i, j);
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

    #[test]
    fn shuffles_match_python() {
        // Six shuffles of [candidate, reference] with Random(280): first names c c r c c c.
        let mut r = PyRandom::new(280);
        let firsts: String = (0..6)
            .map(|_| {
                let mut order = ['c', 'r'];
                r.shuffle(&mut order);
                order[0]
            })
            .collect();
        assert_eq!(firsts, "ccrccc");
    }

    #[test]
    fn floats_match_python() {
        // python3 -c "import random; r=random.Random(5); print([r.random() for _ in range(3)])"
        let mut r = PyRandom::new(5);
        assert_eq!(
            [r.random(), r.random(), r.random()],
            [0.6229016948897019, 0.7417869892607294, 0.7951935655656966]
        );
    }
}
