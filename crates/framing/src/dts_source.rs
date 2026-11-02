use triergon_core::SourceId;

/// Caller-supplied identity for a complete owned DTS source record. Individual
/// jobs preserve this context; it does not validate a transport request/token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsSourceIdentity {
    pub source: SourceId,
    pub generation: u64,
    pub offset: u64,
}
