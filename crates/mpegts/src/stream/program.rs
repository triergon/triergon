//! A borrowed view of the program map a stream routes by.
use super::{PesRoute, TransportStream};
use crate::psi::{DescriptorNamespace, Descriptors, ElementaryStream};
use triergon_core::Bytes;
use triergon_core::container::{CodecId, MediaKind, TrackId, TrackInfo};

/// The accepted program map the stream routes PES by: program number, version, PCR PID and
/// elementary streams in section order. Borrowed from the stream; reading it allocates nothing.
#[derive(Clone, Copy, Debug)]
pub struct ActiveProgram<'a> {
    stream: &'a TransportStream,
    section: &'a Bytes,
    version: u8,
}

impl<'a> ActiveProgram<'a> {
    /// The 16-bit program number selected for this transport reader.
    pub fn program_number(self) -> u16 {
        self.stream.program
    }
    /// The 5-bit version of the accepted PMT section.
    pub fn version(self) -> u8 {
        self.version
    }
    /// The PID that carries the program's PCR; `None` when the map signals none (0x1FFF).
    pub fn pcr_pid(self) -> Option<u16> {
        self.stream.pcr_pid
    }
    /// The whole accepted PMT section, CRC included.
    pub fn section(self) -> &'a Bytes {
        self.section
    }
    /// The elementary streams in section order; their PIDs are unique.
    pub fn streams(self) -> impl ExactSizeIterator<Item = ProgramStream<'a>> + 'a {
        let namespace = self.stream.descriptor_namespace;
        self.stream
            .routes
            .as_slice()
            .iter()
            .map(move |route| ProgramStream { route, namespace })
    }
}

/// One elementary stream of an [`ActiveProgram`]. Its [`TrackInfo`] is the PMT entry's.
/// The neutral language is the first three-letter ISO 639 code in section order.
/// The descriptor view retains all language codes and roles.
#[derive(Clone, Copy, Debug)]
pub struct ProgramStream<'a> {
    route: &'a PesRoute,
    namespace: DescriptorNamespace,
}

impl<'a> ProgramStream<'a> {
    /// The 13-bit elementary PID.
    pub fn pid(self) -> u16 {
        self.route.pid
    }
    /// The 8-bit PMT stream type that describes the PID’s carriage.
    pub fn stream_type(self) -> u8 {
        self.route.stream_type
    }
    /// The entry's descriptor loop bytes; may be empty.
    pub fn descriptor_bytes(self) -> &'a Bytes {
        &self.route.descriptors
    }
    /// A borrowed traversal of the validated descriptor loop in wire order.
    pub fn descriptors(self) -> Descriptors<'a> {
        Descriptors::new(self.route.descriptors.as_ref())
    }
    /// The PMT entry this stream was routed from, for its codec mapping.
    fn entry(self) -> ElementaryStream {
        ElementaryStream::in_namespace(
            self.route.stream_type,
            self.route.pid,
            self.route.descriptors.clone(),
            self.namespace,
        )
    }
}

impl TrackInfo for ProgramStream<'_> {
    fn id(&self) -> TrackId {
        TrackId(u64::from(self.route.pid))
    }
    fn kind(&self) -> MediaKind {
        self.entry().kind()
    }
    fn codec_tag(&self) -> &[u8] {
        std::slice::from_ref(&self.route.stream_type)
    }
    fn codec(&self) -> CodecId {
        TrackInfo::codec(&self.entry())
    }
    fn configuration(&self) -> &[u8] {
        &[]
    }
    fn language(&self) -> Option<&str> {
        self.descriptors()
            .filter_map(|descriptor| {
                let descriptor = descriptor.ok()?;
                descriptor.languages().ok()?;
                Some(descriptor.data.chunks_exact(4))
            })
            .flatten()
            .find_map(|entry| {
                let code = &entry[..3];
                if code.iter().all(u8::is_ascii_alphabetic) {
                    std::str::from_utf8(code).ok()
                } else {
                    None
                }
            })
    }
}

impl TransportStream {
    /// The accepted program map used for routing. Returns `None` until a PMT for the selected
    /// program is accepted, and again after a PAT change or a discontinuity releases it.
    pub fn active_program(&self) -> Option<ActiveProgram<'_>> {
        // Pairing the source and version added 0.1497% to split PES with the PSI route in place.
        Some(ActiveProgram {
            stream: self,
            section: self.pmt_source.as_ref()?,
            version: self.pmt_version?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceEpochs;
    use triergon_core::Budget;

    #[test]
    fn neutral_track_borrows_the_first_well_formed_language_code() {
        let budget = Budget::new(1024).unwrap();
        for (descriptors, expected) in [
            (b"\x0a\x08eng\x01fra\x02".as_slice(), Some("eng")),
            (b"\x0a\x0c\xff00\x00FrA\x02eng\x01", Some("FrA")),
            (b"\x0a\x03eng", None),
            (b"\x0a\x04\xff00\x00", None),
            (b"\x59\x08eng\x14\x00\x01\x00\x02", None),
            (b"", None),
        ] {
            let route = PesRoute {
                pid: 256,
                stream_type: 0x1b,
                descriptors: Bytes::copy_from_slice(&budget, descriptors).unwrap(),
                epochs: SourceEpochs {
                    clock: 0,
                    configuration: 0,
                },
                continuity: None,
                assembler: None,
                reuse: None,
            };
            let stream = ProgramStream {
                route: &route,
                namespace: DescriptorNamespace::Unspecified,
            };
            assert_eq!(stream.language(), expected);
            assert_eq!(stream.descriptor_bytes().as_ref(), descriptors);
        }
    }
}
