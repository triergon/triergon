//! A borrowed view of the program map a stream routes by.
use super::{PesRoute, TransportStream};
use crate::psi::{DescriptorNamespace, Descriptors};
use triergon_core::Bytes;

/// The accepted program map the stream routes PES by: program number, version, PCR PID and
/// elementary streams in section order. Borrowed from the stream; reading it allocates nothing.
#[derive(Clone, Copy, Debug)]
pub struct ActiveProgram<'a> {
    stream: &'a TransportStream,
    section: &'a Bytes,
    version: u8,
}

impl<'a> ActiveProgram<'a> {
    /// The program number.
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
    /// The interpretation applied to private descriptors.
    pub fn descriptor_namespace(self) -> DescriptorNamespace {
        self.stream.descriptor_namespace
    }
    /// The elementary streams in section order; their PIDs are unique.
    pub fn streams(self) -> impl ExactSizeIterator<Item = ProgramStream<'a>> + 'a {
        self.stream.routes.as_slice().iter().map(ProgramStream)
    }
}

/// One elementary stream of an [`ActiveProgram`].
#[derive(Clone, Copy, Debug)]
pub struct ProgramStream<'a>(&'a PesRoute);

impl<'a> ProgramStream<'a> {
    /// The 13-bit elementary PID.
    pub fn pid(self) -> u16 {
        self.0.pid
    }
    /// The 8-bit stream type.
    pub fn stream_type(self) -> u8 {
        self.0.stream_type
    }
    /// The entry's descriptor loop bytes; may be empty.
    pub fn descriptor_bytes(self) -> &'a Bytes {
        &self.0.descriptors
    }
    /// Iterates the entry's descriptors.
    pub fn descriptors(self) -> Descriptors<'a> {
        Descriptors::new(self.0.descriptors.as_ref())
    }
}

impl TransportStream {
    /// The program map the stream currently routes by; `None` until a PMT for the selected
    /// program is accepted, and again after a PAT change or a discontinuity releases it.
    pub fn active_program(&self) -> Option<ActiveProgram<'_>> {
        Some(ActiveProgram {
            stream: self,
            section: self.pmt_source.as_ref()?,
            version: self.pmt_version?,
        })
    }
}
