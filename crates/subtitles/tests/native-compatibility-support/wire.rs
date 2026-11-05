//! Original private benchmark protocol. Neither library nor worker reads golds.
use crate::Result;

pub const MAX_INPUT: usize = 16 * 1024 * 1024;
pub const MAX_RECORDS: usize = 64;
pub const MAX_PIXELS: usize = 8 * 1024 * 1024;
pub const FOLD_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
pub const FOLD_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Debug)]
pub struct Record {
    pub reset: u8,
    pub pts_known: bool,
    pub pts_raw: i64,
    pub dts_known: bool,
    pub dts_raw: i64,
    pub payload: Vec<u8>,
}
#[derive(Debug)]
pub struct Case {
    pub codec: u8,
    pub flags: u32,
    pub numerator: u32,
    pub denominator: u32,
    pub private: Vec<u8>,
    pub records: Vec<Record>,
}
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or("input offset overflow")?;
        let value = self
            .bytes
            .get(self.at..end)
            .ok_or("truncated private protocol")?;
        self.at = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| "u32 width")?,
        ))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| "i64 width")?,
        ))
    }
}
impl Case {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() > MAX_INPUT + 65536 + 4096 {
            return Err("input limit".into());
        }
        let mut c = Cursor { bytes: raw, at: 0 };
        if c.take(8)? != b"SMBMO001" {
            return Err("private protocol magic".into());
        }
        let codec = c.byte()?;
        if codec != 1 {
            return Err("PGS-only codec declaration".into());
        }
        let flags = c.u32()?;
        if flags & !1 != 0 || (codec != 1 && flags != 0) {
            return Err("configuration flags".into());
        }
        let numerator = c.u32()?;
        let denominator = c.u32()?;
        if numerator == 0
            || denominator == 0
            || numerator > i32::MAX as u32
            || denominator > i32::MAX as u32
        {
            return Err("public native rational bounds".into());
        }
        let size = c.u32()? as usize;
        if size > 65536 {
            return Err("private configuration limit".into());
        }
        let private = c.take(size)?.to_vec();
        let count = c.u32()? as usize;
        if count == 0 || count > MAX_RECORDS {
            return Err("record limit".into());
        }
        let mut records = Vec::with_capacity(count);
        let mut total = 0usize;
        for _ in 0..count {
            let reset = c.byte()?;
            if reset != 0 && reset != 2 {
                return Err("PGS reset must be hard reopen".into());
            }
            let pts_known = c.byte()?;
            let pts_raw = c.i64()?;
            let dts_known = c.byte()?;
            let dts_raw = c.i64()?;
            if pts_known > 1 || dts_known > 1 {
                return Err("clock knownness domain".into());
            }
            let size = c.u32()? as usize;
            total = total.checked_add(size).ok_or("payload overflow")?;
            if total > MAX_INPUT || (reset != 0 && size != 0) || (reset == 0 && size == 0) {
                return Err("payload/reset bound".into());
            }
            records.push(Record {
                reset,
                pts_known: pts_known != 0,
                pts_raw,
                dts_known: dts_known != 0,
                dts_raw,
                payload: c.take(size)?.to_vec(),
            });
        }
        if c.at != raw.len() {
            return Err("trailing protocol bytes".into());
        }
        Ok(Self {
            codec,
            flags,
            numerator,
            denominator,
            private,
            records,
        })
    }
}

/// Fixed little-endian transcript and literal FNV-1a fold, shared with C/Python.
/// Capture is enabled only for functional inspection, never for timed operations.
pub struct EventHeader {
    pub packet: usize,
    pub offset: usize,
    pub consumed: i32,
    pub flush: bool,
    pub pts: Option<i64>,
    pub start: u32,
    pub end: u32,
    pub canvas: [i32; 2],
    pub rectangles: usize,
}
pub struct Sink {
    pub digest: u64,
    pub transcript: Option<Vec<u8>>,
    pub bytes: u64,
    pub packets: u64,
    pub events: u64,
    pub rectangles: u64,
    pub pixels: u64,
    pub palette_bytes: u64,
    pub output_bytes: u64,
}
impl Sink {
    pub fn new(capture: bool) -> Self {
        Self {
            digest: FOLD_OFFSET,
            transcript: capture.then(Vec::new),
            bytes: 0,
            packets: 0,
            events: 0,
            rectangles: 0,
            pixels: 0,
            palette_bytes: 0,
            output_bytes: 0,
        }
    }
    pub fn data(&mut self, raw: &[u8]) {
        for b in raw {
            self.digest = (self.digest ^ u64::from(*b)).wrapping_mul(FOLD_PRIME);
        }
        if let Some(out) = &mut self.transcript {
            out.extend_from_slice(raw);
        }
        self.output_bytes += raw.len() as u64;
    }
    pub fn byte(&mut self, n: u8) {
        self.data(&[n]);
    }
    pub fn u16(&mut self, n: u16) {
        self.data(&n.to_le_bytes());
    }
    pub fn u32(&mut self, n: u32) {
        self.data(&n.to_le_bytes());
    }
    pub fn i32(&mut self, n: i32) {
        self.data(&n.to_le_bytes());
    }
    pub fn u64(&mut self, n: u64) {
        self.data(&n.to_le_bytes());
    }
    pub fn i64(&mut self, n: i64) {
        self.data(&n.to_le_bytes());
    }
    pub fn header(&mut self, c: &Case) {
        self.data(b"SMBOUT02");
        self.byte(c.codec);
        self.u32(c.flags);
        self.u32(c.numerator);
        self.u32(c.denominator);
        self.u32(c.private.len() as u32);
        self.data(&c.private);
        self.u32(c.records.len() as u32);
    }
    pub fn input(&mut self, index: usize, r: &Record) {
        self.byte(b'P');
        self.u32(index as u32);
        self.byte(r.reset);
        self.byte(u8::from(r.pts_known));
        self.i64(r.pts_raw);
        self.byte(u8::from(r.dts_known));
        self.i64(r.dts_raw);
        self.u32(r.payload.len() as u32);
        self.data(&r.payload);
        if r.reset == 0 {
            self.packets += 1;
            self.bytes += r.payload.len() as u64;
        }
    }
    pub fn event(&mut self, header: EventHeader) {
        let EventHeader {
            packet,
            offset,
            consumed,
            flush,
            pts,
            start,
            end,
            canvas,
            rectangles,
        } = header;
        self.byte(b'E');
        self.u32(packet as u32);
        self.u32(offset as u32);
        self.i32(consumed);
        self.byte(u8::from(flush));
        self.u16(0);
        self.byte(u8::from(pts.is_some()));
        self.i64(pts.unwrap_or(0));
        self.u32(start);
        self.u32(end);
        self.i32(canvas[0]);
        self.i32(canvas[1]);
        self.u32(rectangles as u32);
        self.events += 1;
    }
    pub fn rect(
        &mut self,
        order: usize,
        geometry: [i32; 6],
        palette: &[u32],
        indices: &[u8],
    ) -> Result<()> {
        let width = usize::try_from(geometry[2]).map_err(|_| "negative width")?;
        let height = usize::try_from(geometry[3]).map_err(|_| "negative height")?;
        if width.checked_mul(height) != Some(indices.len()) {
            return Err("complete index plane extent".into());
        }
        self.rect_rows(order, geometry, palette, |row| {
            let start = row.checked_mul(width).ok_or("row offset overflow")?;
            indices
                .get(start..start + width)
                .ok_or_else(|| "missing index row".into())
        })
    }
    pub fn rect_rows<'a>(
        &mut self,
        order: usize,
        geometry: [i32; 6],
        palette: &[u32],
        mut row: impl FnMut(usize) -> Result<&'a [u8]>,
    ) -> Result<()> {
        let [_, _, w, h, _, stride] = geometry;
        if w < 0 || h < 0 || stride < w || palette.len() > 256 {
            return Err("output geometry/palette limit".into());
        }
        let pixels = (w as usize)
            .checked_mul(h as usize)
            .ok_or("pixel overflow")?;
        if pixels > MAX_PIXELS || self.pixels + pixels as u64 > MAX_PIXELS as u64 {
            return Err("output pixel extent/limit".into());
        }
        for y in 0..h as usize {
            let indices = row(y)?;
            if indices.len() != w as usize
                || indices.iter().any(|i| usize::from(*i) >= palette.len())
            {
                return Err("output row/index outside palette".into());
            }
        }
        self.u32(order as u32);
        for n in geometry {
            self.i32(n);
        }
        self.u32(palette.len() as u32);
        self.u64(pixels as u64);
        for y in 0..h as usize {
            self.data(row(y)?);
        }
        for word in palette {
            self.u32(*word);
        }
        // Complete visible native ARGB consumer, identical to the reference.
        for y in 0..h as usize {
            for index in row(y)? {
                self.u32(palette[usize::from(*index)]);
            }
        }
        self.rectangles += 1;
        self.pixels += pixels as u64;
        self.palette_bytes += (palette.len() * 4) as u64;
        Ok(())
    }
}
