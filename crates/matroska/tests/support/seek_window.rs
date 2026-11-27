//! A recording seekable source with injected faults, the file it serves and the
//! packet facts compared after seeks, shared by the unit and integration tests.
//! Library names resolve at `crate::`: the library itself, or an integration test
//! that imports them at its root.
// Each test binary uses part of this file.
#![allow(dead_code)]
use crate::TypedPacket;
use crate::ebml_kit::{element, mkv, uint};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use triergon_core::Budget;

pub(crate) fn fixture() -> Vec<u8> {
    let track = element(
        0xae,
        &[
            uint(0xd7, 1),
            uint(0x73c5, 1),
            uint(0x83, 1),
            element(0x86, b"V_AUTHORED/OPAQUE"),
            element(0x63a2, b"exact-private"),
            uint(0x23e383, 40_000_000),
        ]
        .concat(),
    );
    let mut body = element(0x1654ae6b, &track);
    for (at, byte) in [(0, b'a'), (40, b'b'), (80, b'c')] {
        let mut block = vec![0x81, 0, 0, 0x80];
        block.extend_from_slice(&[byte; 32]);
        body.extend_from_slice(&element(
            0x1f43b675,
            &[uint(0xe7, at), element(0xa3, &block)].concat(),
        ));
    }
    mkv(&body)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Read {
        before: u64,
        requested: usize,
        returned: usize,
        used: usize,
    },
    Fault {
        before: u64,
        requested: usize,
        used: usize,
    },
    Seek {
        request: String,
        after: u64,
        used: usize,
    },
}

#[derive(Debug)]
pub(crate) struct Source {
    pub(crate) bytes: Cursor<Vec<u8>>,
    pub(crate) chunk: usize,
    pub(crate) pool: Budget,
    pub(crate) log: Arc<Mutex<Vec<Call>>>,
    pub(crate) mode: Arc<AtomicU8>,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl Read for Source {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let before = self.bytes.position();
        let requested = out.len();
        let mode = self.mode.swap(0, Ordering::Relaxed);
        if mode == 1 || mode == 3 {
            self.cancel.store(true, Ordering::Relaxed);
        }
        if mode == 2 || mode == 3 {
            self.log.lock().unwrap().push(Call::Fault {
                before,
                requested,
                used: self.pool.used(),
            });
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "authored transport",
            ));
        }
        let count = out.len().min(self.chunk);
        let returned = self.bytes.read(&mut out[..count])?;
        self.log.lock().unwrap().push(Call::Read {
            before,
            requested,
            returned,
            used: self.pool.used(),
        });
        Ok(returned)
    }
}

impl Seek for Source {
    fn seek(&mut self, at: SeekFrom) -> std::io::Result<u64> {
        let result = self.bytes.seek(at)?;
        self.log.lock().unwrap().push(Call::Seek {
            request: format!("{at:?}"),
            after: result,
            used: self.pool.used(),
        });
        Ok(result)
    }
}

pub(crate) fn source(
    raw: Vec<u8>,
    chunk: usize,
    pool: &Budget,
    cancel: &Arc<AtomicBool>,
) -> (Source, Arc<Mutex<Vec<Call>>>, Arc<AtomicU8>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mode = Arc::new(AtomicU8::new(0));
    (
        Source {
            bytes: Cursor::new(raw),
            chunk,
            pool: pool.clone(),
            log: log.clone(),
            mode: mode.clone(),
            cancel: cancel.clone(),
        },
        log,
        mode,
    )
}

pub(crate) fn facts(packet: &TypedPacket) -> (Option<i64>, Vec<u8>, Vec<u8>) {
    (
        packet.packet.pts_ns,
        packet.packet.data.as_ref().to_vec(),
        packet.configuration.tracks()[0]
            .codec_private
            .as_ref()
            .to_vec(),
    )
}

pub(crate) fn policy_cases() -> [crate::PayloadOwnership; 2] {
    [
        crate::PayloadOwnership::OwnedExact,
        crate::PayloadOwnership::SharedBounded,
    ]
}
