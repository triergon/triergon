//! Shared arguments and charged preloading for MPEG-TS worker modes.

use crate::runner::{MAX_INPUT, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceCapabilities, SourceId};
use triergon_mpegts::PacketFormat;
use triergon_mpegts::psi::{DescriptorNamespace, DvbNamespaceEvidence};

/// Bound the preloaded entity list independently of the byte budget.
const MAX_ENTITIES: usize = 64;
pub(crate) const FIRST_SOURCE: SourceId = SourceId(1);

pub(crate) struct Entity {
    pub(crate) source: SourceId,
    pub(crate) input: Bytes,
    pub(crate) discontinuity: bool,
    position_base: u64,
}

pub(crate) fn capabilities(input: &Bytes) -> SourceCapabilities {
    SourceCapabilities {
        length: Some(input.len() as u64),
        random_access: false,
    }
}

/// A preloaded sequence of actual media entities, all charged to one pool.
pub(crate) struct Request {
    pub(crate) format: PacketFormat,
    pub(crate) program: u16,
    pub(crate) namespace: DescriptorNamespace,
    /// The first physical entity, with source ID 1.
    pub(crate) input: Bytes,
    pub(crate) following: BudgetVec<Entity>,
}

impl Request {
    pub(crate) fn position_base(&self, source: SourceId) -> Option<u64> {
        if source == FIRST_SOURCE {
            return Some(0);
        }
        let index = usize::try_from(source.0.checked_sub(2)?).ok()?;
        self.following
            .as_slice()
            .get(index)
            .map(|entity| entity.position_base)
    }
}

pub(crate) fn parse(pool: &Budget, path: &Path, arguments: &[String]) -> Result<Request> {
    let usage = "expected SIZE PROGRAM [--dvb|--atsc] [[--discontinuity] ENTITY]...";
    let [size, program, rest @ ..] = arguments else {
        return Err(usage.into());
    };
    let format = match size.as_str() {
        "188" => PacketFormat::Ts188,
        "192" => PacketFormat::M2ts192,
        _ => return Err(usage.into()),
    };
    let program: u16 = program.parse()?;
    let (namespace, rest) = match rest {
        [option, rest @ ..] if option == "--dvb" => (
            DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
            rest,
        ),
        [option, rest @ ..] if option == "--atsc" => (DescriptorNamespace::Atsc, rest),
        _ => (DescriptorNamespace::Unspecified, rest),
    };
    let input = load_entity(pool, path, MAX_INPUT)?;
    let mut remaining = MAX_INPUT - input.len() as u64;
    let mut following = BudgetVec::new(pool);
    let mut rest = rest.iter();
    while let Some(argument) = rest.next() {
        if following.len() == MAX_ENTITIES - 1 {
            return Err(Error::Policy("TS media entity count").into());
        }
        let discontinuity = argument == "--discontinuity";
        let name = if discontinuity {
            rest.next().ok_or(usage)?
        } else {
            argument
        };
        let position_base = MAX_INPUT - remaining;
        let bytes = load_entity(pool, &entity(path, name)?, remaining)?;
        remaining -= bytes.len() as u64;
        let source = SourceId(2 + following.len() as u64);
        following
            .try_push(Entity {
                source,
                input: bytes,
                discontinuity,
                position_base,
            })
            .map_err(|error| error.error)?;
    }
    Ok(Request {
        format,
        program,
        namespace,
        input,
        following,
    })
}

fn load_entity(pool: &Budget, path: &Path, remaining: u64) -> Result<Bytes> {
    let metadata = path.metadata()?;
    let size = metadata.len();
    if !metadata.is_file() || size == 0 || size > remaining {
        return Err("input must be a bounded nonempty file".into());
    }
    let mut file = std::fs::File::open(path)?;
    let mut input = BudgetVec::try_filled(pool, 0, usize::try_from(size)?)?;
    file.read_exact(input.as_mut_slice())?;
    if file.read(&mut [0])? != 0 {
        return Err("input grew beyond its declared extent".into());
    }
    Ok(input.freeze().map_err(|error| error.error)?)
}

/// A further media entity, named relative to the first input's directory.
fn entity(first: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.starts_with('-') || name.contains(['/', '\\']) || name == ".." {
        return Err(format!("entity must be a file name next to the input: {name}").into());
    }
    Ok(first.with_file_name(name))
}
