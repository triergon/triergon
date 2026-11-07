//! Authored synthetic scripts and public-API observations for all 125 cases.

use std::collections::BTreeSet;

use super::json::{self, Value};
use super::sha256;

#[path = "oracle.rs"]
mod oracle;

pub(super) const MEMBERSHIP_SHA256: &str =
    "fa3bda4fd8e3fe6d2a6ddb082054f11af0537feb6afbe82a8b46266ff680f3f0";

pub(super) struct Case {
    pub id: String,
    pub bytes: Vec<u8>,
    #[cfg(test)]
    pub once: oracle::Work,
    #[cfg(test)]
    pub expected: oracle::Work,
}

/// Verify the exact original membership, input bytes, observations and both folds.
pub(super) fn load() -> Result<Vec<Case>, String> {
    let parse = |text| json::parse_strict(text).map_err(|e| e.to_string());
    let corpus = parse(include_str!("corpus.json"))?;
    let inputs = parse(include_str!("inputs.json"))?;
    let observations = parse(include_str!("observations.json"))?;
    let contract = parse(include_str!("contract.json"))?;
    let provenance = parse(include_str!("provenance.json"))?;
    let rows = oracle::array(&corpus, "cases")?;
    let scripts = oracle::array(&inputs, "cases")?;
    let profiles = oracle::array(&contract, "required_profiles")?;
    if rows.len() != 125 || scripts.len() != 125 || profiles.len() != 125 {
        return Err("packet framing requires all 125 cases".into());
    }
    if contract.get("work_fields")
        != Some(&Value::strings(["packets", "bytes", "digest", "digest_b"]))
    {
        return Err("packet framing requires both complete work folds".into());
    }
    let golds = observations
        .get("observations")
        .ok_or("missing observations")?;
    let mut cases = Vec::with_capacity(125);
    let (mut ids, mut hashes, mut observed) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let mut binding = String::new();
    for ((row, script), profile) in rows.iter().zip(scripts).zip(profiles) {
        let id = oracle::text(row, "id")?;
        let hash = oracle::text(row, "sha256")?;
        let gold_hash = oracle::text(row, "observation_sha256")?;
        if oracle::text(script, "id")? != id
            || profile.as_str() != Some(id)
            || row.get("profile") != Some(&Value::str(id))
            || oracle::text(row, "file")? != format!("{id}.bsf")
            || oracle::text(row, "rights")? != "authored-synthetic"
            || row.get("required") != Some(&Value::Bool(true))
            || id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            || !ids.insert(id)
            || !hashes.insert(hash)
        {
            return Err(format!("invalid or duplicate framing membership: {id}"));
        }
        let bytes = oracle::hex(oracle::text(script, "hex")?)?;
        if sha256::hex(&bytes) != hash {
            return Err(format!("{id}: input hash changed"));
        }
        let trace = oracle::text(golds, gold_hash)?;
        if sha256::hex(trace.as_bytes()) != gold_hash {
            return Err(format!("{id}: observation hash changed"));
        }
        observed.insert(gold_hash);
        let once = oracle::Work::parse(row.get("expected_once").ok_or("expected_once missing")?)?;
        let expected = oracle::Work::parse(row.get("expected").ok_or("expected missing")?)?;
        if oracle::trace(trace)? != once || oracle::repeated(once, 1)? != expected {
            return Err(format!("{id}: work differs from the recorded observations"));
        }
        binding.push_str(&format!(
            "{id}\t{hash}\t{gold_hash}\t{}\t{}\t{}\t{}\n",
            once.packets, once.bytes, once.digest, once.digest_b
        ));
        cases.push(Case {
            id: id.to_owned(),
            bytes,
            #[cfg(test)]
            once,
            #[cfg(test)]
            expected,
        });
    }
    if observed.len() != 106
        || sha256::hex(binding.as_bytes()) != MEMBERSHIP_SHA256
        || oracle::text(&provenance, "membership_sha256")? != MEMBERSHIP_SHA256
    {
        return Err("packet framing membership differs from the original 125 cases".into());
    }
    Ok(cases)
}
