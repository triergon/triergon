//! H.222.0 2.4.4/2.6 and Annex A; DVB descriptor framing from EN 300 468.
mod classification;
mod codec;
mod descriptors;
pub(crate) mod dvb;
mod pat_table;
mod section;
mod signalling;
mod tables;

pub(crate) use codec::PES_PRIVATE_DATA;
pub use descriptors::{Descriptor, Descriptors};
pub use dvb::{
    CodecIdentification, DescriptorNamespace, DvbNamespaceEvidence, DvbSubtitling,
    DvbSubtitlingService, DvbSubtitlingServices, DvbSubtitlingSignalling,
};
pub use pat_table::{PatCollector, PatSourceRange, PatTable};
pub use section::{PsiCrc32, Section, SectionAssembler};
pub use signalling::{
    AvcVideo, DvbAc3, DvbEac3, HevcVideo, LanguageEntries, LanguageEntry, Registration,
};
pub use tables::{
    ElementaryStream, PatReader, PatSection, PmtReader, ProgramAssociation, ProgramMap,
    SectionHeader, TableStep,
};
