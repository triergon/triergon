//! FFprobe 9.0.2 packet records of the MP4 terminal-media inputs.
//!
//! FFprobe read each input with `-fflags +noparse+nofillin -show_packets
//! -show_data -of json`, once from a nonseekable pipe and once from the file.
//! `fixtures/native_terminal_golds.rs` is generated from those captures by
//! `cargo xtask playback-golds`; `fixtures/terminal-matrix/README.md` names the
//! evidence set and how to regenerate it.
// Each test binary uses part of this module.
#![allow(dead_code)]

/// One FFprobe packet object. Every field keeps FFprobe's own spelling, and an
/// absent key is `None`; `data` holds the bytes of its hex dump.
#[derive(Debug, PartialEq, Eq)]
pub struct Packet {
    pub codec_type: &'static str,
    pub stream_index: usize,
    pub pts: Option<i64>,
    pub pts_time: Option<&'static str>,
    pub dts: Option<i64>,
    pub dts_time: Option<&'static str>,
    pub duration: Option<i64>,
    pub duration_time: Option<&'static str>,
    pub size: &'static str,
    pub pos: Option<&'static str>,
    pub flags: &'static str,
    pub data: &'static [u8],
}

impl Packet {
    /// The source offset FFprobe reports in `pos`.
    pub fn offset(&self) -> u64 {
        self.pos
            .expect("FFprobe position")
            .parse()
            .expect("decimal position")
    }

    /// The duration in stream ticks, zero when FFprobe prints none.
    pub fn duration_ticks(&self) -> i64 {
        self.duration.unwrap_or(0)
    }

    /// FFprobe's `K`, `C` and `D` flags as packet flag bits (key 1, corrupt 2,
    /// discard 4).
    pub fn flag_bits(&self) -> u32 {
        [('K', 1), ('C', 2), ('D', 4)]
            .iter()
            .filter(|(flag, _)| self.flags.contains(*flag))
            .map(|(_, bit)| bit)
            .sum()
    }
}

/// What FFprobe printed for one read of an input.
#[derive(Debug)]
pub enum Probe {
    /// It exited 0 after these packets.
    Packets(&'static [Packet]),
    /// It exited 1 after `error reading header` with AVERROR_INVALIDDATA, and
    /// printed no packets.
    InvalidHeader,
}

/// FFprobe's read of the file, recorded for the 31 original inputs, which the
/// matrix also reads from a seekable source.
#[derive(Debug)]
pub enum File {
    SameAsPipe,
    Own(Probe),
}

pub struct Case {
    pub name: &'static str,
    pub raw: &'static [u8],
    pub pipe: Probe,
    pub file: Option<File>,
}

impl Case {
    /// The packets FFprobe printed from the pipe.
    pub fn packets(&self) -> &'static [Packet] {
        match self.pipe {
            Probe::Packets(packets) => packets,
            Probe::InvalidHeader => panic!("{}: FFprobe refused the pipe", self.name),
        }
    }

    /// FFprobe's seekable read, for an input the matrix reads from a file.
    pub fn seekable(&self) -> Option<&Probe> {
        self.file.as_ref().map(|file| match file {
            File::SameAsPipe => &self.pipe,
            File::Own(probe) => probe,
        })
    }
}

/// The input called `name`.
pub fn case(name: &str) -> &'static Case {
    CASES
        .iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("no FFprobe record for {name}"))
}

include!("../fixtures/native_terminal_golds.rs");
