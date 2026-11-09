//! Feature contracts registered under `harness/features`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::checks::read_json;
use crate::json::Value;

/// When a feature runs: its contract's `gate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// `"required"`: `cargo xtask check` requires it.
    Required,
    /// `"on-demand"`: run by `COMMAND compare --all`.
    OnDemand,
    /// `"diagnostic"`: records where another reference departs from the gated
    /// one; run by `COMMAND compare --diagnostic`.
    Diagnostic,
}

impl Gate {
    /// The gate a contract's `gate` names.
    fn named(name: &str) -> Option<Self> {
        match name {
            "required" => Some(Self::Required),
            "on-demand" => Some(Self::OnDemand),
            "diagnostic" => Some(Self::Diagnostic),
            _ => None,
        }
    }
}

/// The contract's scheduling rule, when it declares one.
pub(crate) fn gate(id: &str, document: &Value) -> Result<Option<Gate>, String> {
    document
        .get("gate")
        .map(|value| {
            value
                .as_str()
                .and_then(Gate::named)
                .ok_or_else(|| format!("{id}: gate must be required, on-demand or diagnostic"))
        })
        .transpose()
}

pub(crate) struct Contract {
    pub id: String,
    pub path: PathBuf,
    pub document: Value,
}

impl Contract {
    pub fn candidate(&self) -> Option<&str> {
        self.document.get("candidate").and_then(Value::as_str)
    }

    pub fn manifest(&self) -> PathBuf {
        self.path.with_file_name("corpus.json")
    }

    /// Generated inputs can also have a materialized manifest for inspection.
    pub fn generated(&self) -> bool {
        self.path.with_file_name("pins.json").is_file()
            || self.document.get("cases_from").is_some()
            || !self.manifest().is_file()
    }
}

/// Read the registered contracts in ID order.
pub(crate) fn discover(root: &Path) -> Result<Vec<Contract>, String> {
    let directory = root.join("harness/features");
    let entries = fs::read_dir(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
    let mut contracts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", directory.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{}: invalid feature directory name", entry.path().display()))?;
        let path = entry.path().join("contract.json");
        let document = read_json(&path)?;
        if document.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return Err(format!("{}: expected feature id {id}", path.display()));
        }
        contracts.push(Contract { id, path, document });
    }
    contracts.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    Ok(contracts)
}

/// Features the workspace check runs, including those with committed manifests.
pub(crate) fn required(root: &Path) -> Result<Vec<Contract>, String> {
    let mut required = Vec::new();
    for contract in discover(root)? {
        if gate(&contract.id, &contract.document)? == Some(Gate::Required) {
            required.push(contract);
        }
    }
    Ok(required)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_discovers_a_required_contract_with_a_committed_manifest() {
        let root = crate::walk::scratch("required-contracts");
        let create = |id: &str, gate: &str| {
            let directory = root.join("harness/features").join(id);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("contract.json"),
                format!(r#"{{"id":"{id}","gate":"{gate}","candidate":"some-worker","reference":"some-reference"}}"#),
            )
            .unwrap();
            fs::write(directory.join("corpus.json"), "{}").unwrap();
        };
        create("required-feature", "required");
        create("extra-feature", "on-demand");
        let contracts = required(&root).unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].id, "required-feature");
        assert_eq!(contracts[0].candidate(), Some("some-worker"));
        assert!(contracts[0].manifest().is_file());
        assert!(!contracts[0].generated());
        fs::remove_dir_all(root).unwrap();
    }
}
