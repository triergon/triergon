//! Residue decode (Vorbis I section 8): partitioned, classified VQ of the spectral fine
//! structure.

use triergon_bitstream::LsbReader;
use triergon_codec::{Budget, BudgetVec};

use crate::codebook::Codebook;
use crate::spectra::Spectra;
use crate::{DecodeError, push};

/// How a residue lays its VQ vectors out over the coded vectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// Type 0: the scalars of each VQ vector are spread across its partition.
    Strided,
    /// Type 1: VQ vectors follow each other within a partition.
    Sequential,
    /// Type 2: type 1 over one vector interleaving all channels of the submap.
    Interleaved,
}

#[derive(Debug)]
pub(crate) struct Residue {
    format: Format,
    begin: usize,
    end: usize,
    partition_size: usize,
    classbook: u8,
    /// The book of each pass, per classification.
    books: BudgetVec<[Option<u8>; 8]>,
    /// One more than the last pass any classification codes; at least 1, as pass 0 also
    /// reads the classifications.
    passes: usize,
    /// How far the last VQ vector of a partition may reach past it: the largest
    /// dimension of a book that does not divide the partition size, else 0.
    overhang: usize,
}

/// A channel of a submap and whether its residue is coded in this packet.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Member {
    pub(crate) channel: u8,
    pub(crate) coded: bool,
}

/// The vectors one residue decode fills.
trait Bundle {
    fn count(&self) -> usize;
    fn coded(&self, vector: usize) -> bool;
    fn get(&mut self, vector: usize) -> &mut [f32];
}

/// Type 2's single interleaved vector, always coded.
struct Single<'a>(&'a mut [f32]);

impl Bundle for Single<'_> {
    fn count(&self) -> usize {
        1
    }
    fn coded(&self, _: usize) -> bool {
        true
    }
    fn get(&mut self, _: usize) -> &mut [f32] {
        self.0
    }
}

/// One vector per member channel.
struct Members<'a> {
    spectra: &'a mut Spectra,
    members: &'a [Member],
    len: usize,
}

impl Bundle for Members<'_> {
    fn count(&self) -> usize {
        self.members.len()
    }
    fn coded(&self, vector: usize) -> bool {
        self.members[vector].coded
    }
    fn get(&mut self, vector: usize) -> &mut [f32] {
        let channel = usize::from(self.members[vector].channel);
        &mut self.spectra.channel_mut(channel)[..self.len]
    }
}

impl Residue {
    /// Reads a residue configuration of type 0, 1 or 2 (section 8.6.1).
    pub(crate) fn read(
        r: &mut LsbReader<'_>,
        codebooks: &[Codebook],
        budget: &Budget,
    ) -> Result<Residue, DecodeError> {
        let format = match r.read(16) {
            0 => Format::Strided,
            1 => Format::Sequential,
            2 => Format::Interleaved,
            _ => return Err(DecodeError::Invalid("residue type")),
        };
        let begin = r.read(24) as usize;
        let end = r.read(24) as usize;
        let partition_size = r.read(24) as usize + 1;
        let classifications = r.read(6) + 1;
        let classbook = r.read(8) as u8;
        let mut cascade = [0u32; 64];
        let cascade = &mut cascade[..classifications as usize];
        for pattern in &mut *cascade {
            let low = r.read(3);
            let high = if r.bit() { r.read(5) } else { 0 };
            *pattern = high << 3 | low;
        }
        let mut books = BudgetVec::new(budget);
        books.try_reserve_exact(cascade.len())?;
        for &pattern in &*cascade {
            let mut passes = [None; 8];
            for (pass, slot) in passes.iter_mut().enumerate() {
                if pattern & 1 << pass != 0 {
                    let book = r.read(8) as u8;
                    if !codebooks
                        .get(usize::from(book))
                        .is_some_and(Codebook::has_vectors)
                    {
                        return Err(DecodeError::Invalid("residue codebook"));
                    }
                    *slot = Some(book);
                }
            }
            push(&mut books, passes)?;
        }
        if r.overrun() {
            return Err(DecodeError::Invalid("truncated residue"));
        }
        let classes = codebooks
            .get(usize::from(classbook))
            .ok_or(DecodeError::Invalid("residue classbook"))?;
        let per_word = classes.dimensions();
        let words = (0..per_word).try_fold(1u64, |words, _| {
            Some(words * u64::from(classifications)).filter(|&w| w <= u64::from(classes.entries()))
        });
        if per_word == 0 || words.is_none() {
            return Err(DecodeError::Invalid("residue classbook"));
        }
        let overhang = books
            .iter()
            .flatten()
            .flatten()
            .map(|&book| codebooks[usize::from(book)].dimensions())
            .filter(|&dimensions| {
                format != Format::Strided && !partition_size.is_multiple_of(dimensions)
            })
            .max()
            .unwrap_or(0);
        let passes = books
            .iter()
            .filter_map(|passes| passes.iter().rposition(Option::is_some))
            .max()
            .map_or(1, |last| last + 1);
        Ok(Residue {
            format,
            begin,
            end,
            partition_size,
            classbook,
            books,
            passes,
            overhang,
        })
    }

    /// Classification storage a decode of up to `channels` vectors of `half` values each
    /// may need.
    pub(crate) fn class_slots(&self, channels: usize, half: usize) -> usize {
        let partitions = |len: usize| {
            self.end.min(len).saturating_sub(self.begin.min(len)) / self.partition_size
        };
        match self.format {
            Format::Interleaved => partitions(channels.saturating_mul(half)),
            _ => channels.saturating_mul(partitions(half)),
        }
    }

    /// True for type 2, which decodes through an interleaved vector.
    pub(crate) fn interleaves(&self) -> bool {
        self.format == Format::Interleaved
    }

    /// Decodes the residue vectors of the `members` of a submap into the first `half`
    /// values of their `spectra` (section 8.6.2), which must be zero on entry. Returns
    /// how many leading values of each vector decode may have set; the rest stay zero.
    pub(crate) fn decode(
        &self,
        r: &mut LsbReader<'_>,
        codebooks: &[Codebook],
        half: usize,
        members: &[Member],
        spectra: &mut Spectra,
        scratch: &mut Scratch,
    ) -> usize {
        if !members.iter().any(|member| member.coded) {
            return 0;
        }
        let count = members.len();
        if self.format == Format::Interleaved {
            let extent = self.reach(count * half).div_ceil(count).min(half);
            let vector = &mut scratch.interleaved[..count * half];
            vector[..extent * count].fill(0.0);
            self.decode_bundle(r, codebooks, &mut Single(vector), &mut scratch.classes);
            deinterleave(&vector[..extent * count], members, half, spectra);
            extent
        } else {
            let mut bundle = Members {
                spectra,
                members,
                len: half,
            };
            self.decode_bundle(r, codebooks, &mut bundle, &mut scratch.classes);
            self.reach(half)
        }
    }

    /// The first value and the number of partitions decode codes in a vector of `len`.
    fn coded(&self, len: usize) -> (usize, usize) {
        let begin = self.begin.min(len);
        (
            begin,
            self.end.min(len).saturating_sub(begin) / self.partition_size,
        )
    }

    /// The end of the values decode may set in a vector of `len`.
    fn reach(&self, len: usize) -> usize {
        let (begin, partitions) = self.coded(len);
        if partitions == 0 {
            return 0;
        }
        (begin + partitions * self.partition_size + self.overhang).min(len)
    }

    fn decode_bundle(
        &self,
        r: &mut LsbReader<'_>,
        codebooks: &[Codebook],
        vectors: &mut impl Bundle,
        classes: &mut [u8],
    ) {
        let (begin, partitions) = self.coded(vectors.get(0).len());
        if partitions == 0 {
            return;
        }
        let classbook = &codebooks[usize::from(self.classbook)];
        let per_word = classbook.dimensions();
        let count = vectors.count();
        let Some(classes) = classes.get_mut(..count * partitions) else {
            return;
        };
        for pass in 0..self.passes {
            let mut start = 0;
            while start < partitions {
                let stop = (start + per_word).min(partitions);
                if pass == 0 {
                    for (j, rows) in classes.chunks_exact_mut(partitions).enumerate() {
                        if !vectors.coded(j) {
                            continue;
                        }
                        let Some(word) = classbook.scalar(r) else {
                            return;
                        };
                        self.unpack_classes(word, per_word, &mut rows[start..stop]);
                    }
                }
                for partition in start..stop {
                    let offset = begin + partition * self.partition_size;
                    for (j, row) in classes.chunks_exact(partitions).enumerate() {
                        if !vectors.coded(j) {
                            continue;
                        }
                        let Some(book) = self.books[usize::from(row[partition])][pass] else {
                            continue;
                        };
                        let book = &codebooks[usize::from(book)];
                        let out = &mut vectors.get(j)[offset..];
                        if !self.read_partition(book, r, out) {
                            return;
                        }
                    }
                }
                start = stop;
            }
        }
    }

    /// The classifications of one codeword, most significant digit first, for the
    /// partitions still to read.
    fn unpack_classes(&self, mut word: u32, per_word: usize, slots: &mut [u8]) {
        let classifications = self.books.len() as u32;
        if classifications == 1 {
            slots.fill(0);
            return;
        }
        // At most 24 digits: `classifications^per_word` fits the classbook's entries.
        for digit in (0..per_word).rev() {
            if let Some(slot) = slots.get_mut(digit) {
                *slot = (word % classifications) as u8;
            }
            word /= classifications;
        }
    }

    /// Adds one partition's VQ vectors to `out`, which starts at the partition and runs
    /// to the end of its vector. False if a codeword does not decode.
    #[inline]
    fn read_partition(&self, book: &Codebook, r: &mut LsbReader<'_>, out: &mut [f32]) -> bool {
        let size = self.partition_size;
        let dimensions = book.dimensions();
        if self.format == Format::Strided {
            let step = size / dimensions;
            let out = &mut out[..size];
            let mut offset = 0;
            return book.vectors(r, step, |vector| {
                for (slot, &value) in out[offset..].iter_mut().step_by(step).zip(vector) {
                    *slot += value;
                }
                offset += 1;
            });
        }
        if !size.is_multiple_of(dimensions) {
            // A last vector that overhangs the partition spills into the next.
            let mut chunks = out.chunks_mut(dimensions).take(size.div_ceil(dimensions));
            return book.vectors(r, chunks.len(), |vector| add(chunks.next(), vector));
        }
        let out = &mut out[..size];
        match dimensions {
            1 => add_vectors::<1>(book, r, out),
            2 => add_vectors::<2>(book, r, out),
            3 => add_vectors::<3>(book, r, out),
            4 => add_vectors::<4>(book, r, out),
            5 => add_vectors::<5>(book, r, out),
            8 => add_vectors::<8>(book, r, out),
            _ => {
                let mut chunks = out.chunks_exact_mut(dimensions);
                book.vectors(r, chunks.len(), |vector| add(chunks.next(), vector))
            }
        }
    }
}

/// Adds `vector` to `chunk`, if there is one.
#[inline(always)]
fn add(chunk: Option<&mut [f32]>, vector: &[f32]) {
    for (slot, &value) in chunk.into_iter().flatten().zip(vector) {
        *slot += value;
    }
}

/// Adds consecutive VQ vectors of `book`, of `D` values each, over `out`. False if a
/// codeword does not decode.
#[inline(always)]
fn add_vectors<const D: usize>(book: &Codebook, r: &mut LsbReader<'_>, out: &mut [f32]) -> bool {
    let mut chunks = out.chunks_exact_mut(D);
    book.vectors(r, chunks.len(), |vector| {
        if let (Some(chunk), Ok(vector)) = (chunks.next(), <&[f32; D]>::try_from(vector)) {
            for (slot, &value) in chunk.iter_mut().zip(vector) {
                *slot += value;
            }
        }
    })
}

/// Spreads type 2's interleaved vector over the members' spectra.
fn deinterleave(vector: &[f32], members: &[Member], half: usize, spectra: &mut Spectra) {
    match members.len() {
        2 => spread::<2>(vector, members, half, spectra),
        3 => spread::<3>(vector, members, half, spectra),
        4 => spread::<4>(vector, members, half, spectra),
        5 => spread::<5>(vector, members, half, spectra),
        6 => spread::<6>(vector, members, half, spectra),
        7 => spread::<7>(vector, members, half, spectra),
        8 => spread::<8>(vector, members, half, spectra),
        count => {
            for (j, member) in members.iter().enumerate() {
                let channel = &mut spectra.channel_mut(usize::from(member.channel))[..half];
                for (out, frame) in channel.iter_mut().zip(vector.chunks_exact(count)) {
                    *out = frame[j];
                }
            }
        }
    }
}

/// [`deinterleave`] for `C` members, frame by frame.
fn spread<const C: usize>(vector: &[f32], members: &[Member], half: usize, spectra: &mut Spectra) {
    let channels: [usize; C] = core::array::from_fn(|j| usize::from(members[j].channel));
    let Some(mut channels) = spectra.disjoint_mut(channels) else {
        return;
    };
    for (i, frame) in vector.chunks_exact(C).take(half).enumerate() {
        for (channel, &value) in channels.iter_mut().zip(frame) {
            channel[i] = value;
        }
    }
}

/// Working storage of residue decode, sized at setup.
#[derive(Debug)]
pub(crate) struct Scratch {
    /// Type 2's interleaved vector.
    pub(crate) interleaved: BudgetVec<f32>,
    /// Classification of each partition of each vector.
    pub(crate) classes: BudgetVec<u8>,
}
