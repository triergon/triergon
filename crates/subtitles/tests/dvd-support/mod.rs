//! Authored DVD subpicture units, sectors and VobSub text for the DVD tests.
#![allow(dead_code, reason = "each test file uses part of the builders")]

/// One display control command.
#[derive(Clone, Copy, Debug)]
pub enum Cmd {
    Forced,
    Start,
    Stop,
    /// Colour selectors for pixel values 3, 2, 1, 0.
    Palette([u8; 4]),
    /// Contrast for pixel values 3, 2, 1, 0.
    Alpha([u8; 4]),
    /// The image's own area.
    Area,
    /// An explicit area: columns and rows, inclusive.
    AreaAt(u16, u16, u16, u16),
    /// The image's own field offsets.
    Offsets,
    OffsetsAt(u16, u16),
    Raw(&'static [u8]),
}

pub const SHOW: [Cmd; 5] = [
    Cmd::Palette([3, 2, 1, 0]),
    Cmd::Alpha([15, 15, 15, 0]),
    Cmd::Area,
    Cmd::Offsets,
    Cmd::Start,
];

/// Big-endian nibbles.
#[derive(Default)]
pub struct Nibbles {
    pub bytes: Vec<u8>,
    half: Option<u8>,
}

impl Nibbles {
    pub fn put(&mut self, value: u16, count: u32) {
        for shift in (0..count).rev() {
            let nibble = (value >> (4 * shift)) as u8 & 15;
            match self.half.take() {
                Some(high) => self.bytes.push(high << 4 | nibble),
                None => self.half = Some(nibble),
            }
        }
    }

    pub fn align(&mut self) {
        if let Some(high) = self.half.take() {
            self.bytes.push(high << 4);
        }
    }

    /// One run of `count` pixels (0: to the end of the line).
    pub fn run(&mut self, count: u16, colour: u8) {
        let code = count << 2 | u16::from(colour & 3);
        let nibbles = match count {
            1..=3 => 1,
            4..=15 => 2,
            16..=63 => 3,
            _ => 4,
        };
        self.put(code, nibbles);
    }
}

/// Field data from lines of `(count, colour)` runs.
pub fn field(lines: &[&[(u16, u8)]]) -> Vec<u8> {
    let mut w = Nibbles::default();
    for line in lines {
        for &(count, colour) in *line {
            w.run(count, colour);
        }
        w.align();
    }
    w.bytes
}

/// The top (even rows) and bottom (odd rows) fields of an image.
pub fn fields(rows: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
    let mut out = [Nibbles::default(), Nibbles::default()];
    for (y, row) in rows.iter().enumerate() {
        let w = &mut out[y % 2];
        let mut x = 0;
        while x < row.len() {
            let mut run = 1;
            while x + run < row.len() && row[x + run] == row[x] && run < 255 {
                run += 1;
            }
            w.run(run as u16, row[x]);
            x += run;
        }
        w.align();
    }
    let [top, bottom] = out;
    (top.bytes, bottom.bytes)
}

/// A deterministic text-like image: 0 background, 1 body, 2 outline, 3 shadow,
/// with a transparent border of `border` pixels.
pub fn glyphs(width: usize, height: usize, seed: usize, border: usize) -> Vec<Vec<u8>> {
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| {
                    if x < border || y < border || x >= width - border || y >= height - border {
                        return 0;
                    }
                    let u = (x * 7 + y * 3 + seed * 5) % 23;
                    match (x / 5 + y / 3 + seed) % 7 {
                        0 => 0,
                        _ if u < 3 => 2,
                        _ if u < 5 => 3,
                        _ => 1,
                    }
                })
                .collect()
        })
        .collect()
}

fn area(x1: u16, x2: u16, y1: u16, y2: u16) -> Vec<u8> {
    vec![
        5,
        (x1 >> 4) as u8,
        ((x1 & 15) << 4 | x2 >> 8) as u8,
        x2 as u8,
        (y1 >> 4) as u8,
        ((y1 & 15) << 4 | y2 >> 8) as u8,
        y2 as u8,
    ]
}

fn command(cmd: Cmd, image_area: &[u8], offsets: (u16, u16)) -> Vec<u8> {
    match cmd {
        Cmd::Forced => vec![0],
        Cmd::Start => vec![1],
        Cmd::Stop => vec![2],
        Cmd::Palette([a, b, c, d]) => vec![3, a << 4 | b, c << 4 | d],
        Cmd::Alpha([a, b, c, d]) => vec![4, a << 4 | b, c << 4 | d],
        Cmd::Area => image_area.to_vec(),
        Cmd::AreaAt(x1, x2, y1, y2) => area(x1, x2, y1, y2),
        Cmd::Offsets => command(Cmd::OffsetsAt(offsets.0, offsets.1), image_area, offsets),
        Cmd::OffsetsAt(top, bottom) => {
            let mut out = vec![6];
            out.extend(top.to_be_bytes());
            out.extend(bottom.to_be_bytes());
            out
        }
        Cmd::Raw(bytes) => bytes.to_vec(),
    }
}

/// A unit of `rows` at (`x`, `y`) with these control sequences; each ends with
/// `0xff` and links to the next, the last to itself.
pub fn unit(rows: &[Vec<u8>], x: u16, y: u16, sequences: &[(u16, &[Cmd])]) -> Vec<u8> {
    let (top, bottom) = fields(rows);
    let mut data = top.clone();
    data.extend(&bottom);
    let image_area = area(
        x,
        x + rows[0].len() as u16 - 1,
        y,
        y + rows.len() as u16 - 1,
    );
    let offsets = (4, 4 + top.len() as u16);
    let bodies: Vec<Vec<u8>> = sequences
        .iter()
        .map(|(_, cmds)| {
            let mut body: Vec<u8> = cmds
                .iter()
                .flat_map(|&c| command(c, &image_area, offsets))
                .collect();
            body.push(0xff);
            body
        })
        .collect();
    raw_unit(&data, sequences.iter().map(|(d, _)| *d).zip(bodies))
}

/// A unit of raw pixel `data` and `(date, body)` sequences.
pub fn raw_unit(data: &[u8], sequences: impl IntoIterator<Item = (u16, Vec<u8>)>) -> Vec<u8> {
    let sequences: Vec<_> = sequences.into_iter().collect();
    let control = 4 + data.len();
    let mut starts = Vec::new();
    let mut at = control;
    for (_, body) in &sequences {
        starts.push(at);
        at += 4 + body.len();
    }
    let mut out = Vec::new();
    out.extend((at as u16).to_be_bytes());
    out.extend((control as u16).to_be_bytes());
    out.extend(data);
    for (i, (date, body)) in sequences.iter().enumerate() {
        let next = starts.get(i + 1).copied().unwrap_or(starts[i]);
        out.extend(date.to_be_bytes());
        out.extend((next as u16).to_be_bytes());
        out.extend(body);
    }
    out
}

/// An MPEG-2 pack header.
pub fn pack() -> Vec<u8> {
    vec![0, 0, 1, 0xba, 0x44, 0, 4, 0, 4, 1, 0x01, 0x89, 0xc3, 0xf8]
}

/// A 33-bit time stamp with its marker bits.
pub fn pts_bytes(pts: u64, marker: u8) -> [u8; 5] {
    [
        marker | ((pts >> 29) & 0x0e) as u8,
        (pts >> 22) as u8,
        ((pts >> 14) & 0xfe) as u8 | 1,
        (pts >> 7) as u8,
        ((pts << 1) & 0xfe) as u8 | 1,
    ]
}

/// A PES packet with an MPEG-2 header and an optional PTS.
pub fn pes(stream_id: u8, payload: &[u8], pts: Option<u64>) -> Vec<u8> {
    let mut body = vec![0x81, if pts.is_some() { 0x80 } else { 0 }];
    match pts {
        Some(pts) => {
            body.push(5);
            body.extend(pts_bytes(pts, 0x21));
        }
        None => body.push(0),
    }
    body.extend(payload);
    let mut out = vec![0, 0, 1, stream_id];
    out.extend((body.len() as u16).to_be_bytes());
    out.extend(body);
    out
}

/// A subpicture PES packet of stream `number`.
pub fn subpicture(number: u8, data: &[u8], pts: Option<u64>) -> Vec<u8> {
    let mut payload = vec![0x20 + number];
    payload.extend(data);
    pes(0xbd, &payload, pts)
}

/// A padding packet of exactly `length` bytes.
pub fn padding(length: usize) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0xbe];
    out.extend(((length - 6) as u16).to_be_bytes());
    out.resize(length, 0xff);
    out
}

/// A 2048-byte sector: a pack header, `packets`, then padding.
pub fn sector(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = pack();
    for p in packets {
        out.extend(p);
    }
    let gap = 2048 - out.len();
    if gap != 0 {
        out.extend(padding(gap));
    }
    out
}

/// Big-endian bits.
#[derive(Default)]
pub struct Bits {
    pub bytes: Vec<u8>,
    count: usize,
}

impl Bits {
    pub fn put(&mut self, value: u32, width: u32) {
        for shift in (0..width).rev() {
            if self.count.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let last = self.bytes.len() - 1;
            self.bytes[last] |= ((value >> shift & 1) as u8) << (7 - self.count % 8);
            self.count += 1;
        }
    }

    pub fn align(&mut self) {
        self.count = self.count.div_ceil(8) * 8;
    }
}
