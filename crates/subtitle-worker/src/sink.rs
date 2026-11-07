//! The canonical subtitle output both workers fold and trace.
//!
//! One event is one subtitle the decoder hands its caller: the index of the input
//! packet whose decode call produced it (the packet count for the drain after the
//! last packet), its presentation time in microseconds, the display window in
//! milliseconds relative to it, its format (0 bitmap, 1 text) and its rectangles.
//! A rectangle is a bitmap (type 1: position, size, palette of ARGB words and
//! `w * h` index bytes, row by row), plain text (type 2) or an ASS event line
//! (type 3). Fold order and trace bytes match `harness/reference_subtitles.h`.

use std::io::{self, Write};
use triergon_harness_protocol::digest::{fold, fold_bytes};

/// A subtitle's fields before its rectangles.
#[derive(Clone, Copy, Debug)]
pub struct EventHead {
    pub packet: u64,
    /// Microseconds, `None` when unknown (folded as `i64::MIN`).
    pub pts: Option<i64>,
    pub start_ms: u32,
    pub end_ms: u32,
    pub format: u16,
    pub rects: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RectKind {
    Bitmap = 1,
    Text = 2,
    Ass = 3,
}

/// A rectangle's fields before its palette, pixels and text.
#[derive(Clone, Copy, Debug)]
pub struct RectHead {
    pub kind: RectKind,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub colors: i32,
    pub flags: i32,
}

/// Bitmap content of a rectangle: ARGB words and `h` rows of `w` index bytes.
pub struct Bitmap<'a> {
    pub palette: &'a [u32],
    /// Index bytes with `stride >= w` between row starts.
    pub pixels: &'a [u8],
    pub stride: usize,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Work {
    pub events: u64,
    pub bytes: u64,
    pub digest: u64,
}

pub struct Sink<'a> {
    pub work: Work,
    trace: Option<&'a mut dyn Write>,
    first_rect: bool,
}

impl<'a> Sink<'a> {
    pub fn new(trace: Option<&'a mut dyn Write>) -> Self {
        Self {
            work: Work::default(),
            trace,
            first_rect: true,
        }
    }

    fn text(&mut self, text: Option<&[u8]>) {
        let bytes = text.unwrap_or_default();
        fold(&mut self.work.digest, bytes.len() as u64);
        fold_bytes(&mut self.work.digest, bytes);
        self.work.bytes += bytes.len() as u64;
    }

    /// The decoder's ASS header, once per operation before any event.
    pub fn header(&mut self, packets: usize, header: Option<&[u8]>) -> io::Result<()> {
        self.text(header);
        if let Some(out) = self.trace.as_deref_mut() {
            write!(
                out,
                "{{\"schema\":1,\"operation\":\"subtitle-decode\",\"packets\":{packets},\"header\":"
            )?;
            quote(out, header)?;
            out.write_all(b"}\n")?;
        }
        Ok(())
    }

    /// A packet the decoder refused; traced only, never folded.
    pub fn refused(&mut self, packet: usize, code: i32) -> io::Result<()> {
        if let Some(out) = self.trace.as_deref_mut() {
            writeln!(out, "{{\"packet\":{packet},\"error\":{code}}}")?;
        }
        Ok(())
    }

    pub fn begin(&mut self, head: EventHead) -> io::Result<()> {
        self.work.events += 1;
        let pts = head.pts.unwrap_or(i64::MIN);
        for word in [
            head.packet,
            pts as u64,
            u64::from(head.start_ms),
            u64::from(head.end_ms),
            u64::from(head.format),
            u64::from(head.rects),
        ] {
            fold(&mut self.work.digest, word);
        }
        self.first_rect = true;
        if let Some(out) = self.trace.as_deref_mut() {
            write!(out, "{{\"packet\":{},\"pts\":", head.packet)?;
            match head.pts {
                Some(pts) => write!(out, "{pts}")?,
                None => out.write_all(b"null")?,
            }
            write!(
                out,
                ",\"start\":{},\"end\":{},\"format\":{},\"rects\":[",
                head.start_ms, head.end_ms, head.format
            )?;
        }
        Ok(())
    }

    pub fn rect(
        &mut self,
        head: RectHead,
        bitmap: Option<&Bitmap<'_>>,
        text: Option<&[u8]>,
        ass: Option<&[u8]>,
    ) -> io::Result<()> {
        for word in [
            head.kind as u64,
            head.x as u64,
            head.y as u64,
            head.w as u64,
            head.h as u64,
            head.colors as u64,
            head.flags as u64,
        ] {
            fold(&mut self.work.digest, word);
        }
        let palette = bitmap.map_or(&[][..], |b| b.palette);
        for &word in palette {
            fold(&mut self.work.digest, u64::from(word));
        }
        self.work.bytes += palette.len() as u64 * 4;
        let (w, h) = (
            usize::try_from(head.w).unwrap_or(0),
            usize::try_from(head.h).unwrap_or(0),
        );
        let rows = bitmap.filter(|_| w > 0 && h > 0);
        if let Some(b) = rows {
            for row in 0..h {
                fold_bytes(&mut self.work.digest, &b.pixels[row * b.stride..][..w]);
            }
            self.work.bytes += (w * h) as u64;
        }
        self.text(text);
        self.text(ass);
        if let Some(out) = self.trace.as_deref_mut() {
            if !self.first_rect {
                out.write_all(b",")?;
            }
            write!(
                out,
                "{{\"type\":{},\"x\":{},\"y\":{},\"w\":{},\"h\":{},\"colors\":{},\"flags\":{},\"palette\":\"",
                head.kind as i32, head.x, head.y, head.w, head.h, head.colors, head.flags
            )?;
            for word in palette {
                write!(out, "{word:08x}")?;
            }
            out.write_all(b"\",\"pixels\":\"")?;
            if let Some(b) = rows {
                for row in 0..h {
                    for byte in &b.pixels[row * b.stride..][..w] {
                        write!(out, "{byte:02x}")?;
                    }
                }
            }
            out.write_all(b"\",\"text\":")?;
            quote(out, text)?;
            out.write_all(b",\"ass\":")?;
            quote(out, ass)?;
            out.write_all(b"}")?;
        }
        self.first_rect = false;
        Ok(())
    }

    pub fn end(&mut self) -> io::Result<()> {
        if let Some(out) = self.trace.as_deref_mut() {
            out.write_all(b"]}\n")?;
        }
        Ok(())
    }
}

/// JSON string as the reference worker writes it: `"` and `\` escaped, bytes
/// below 0x20 as `\u00XX`, every other byte unchanged; `null` for no text.
fn quote(out: &mut dyn Write, text: Option<&[u8]>) -> io::Result<()> {
    let Some(text) = text else {
        return out.write_all(b"null");
    };
    out.write_all(b"\"")?;
    for &byte in text {
        match byte {
            b'"' | b'\\' => out.write_all(&[b'\\', byte])?,
            0..0x20 => write!(out, "\\u{byte:04x}")?,
            _ => out.write_all(&[byte])?,
        }
    }
    out.write_all(b"\"")
}
