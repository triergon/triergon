//! The runtime gate of `harness/policy.json`, which feature contracts restate and
//! the codec and capture runners read directly.

use std::path::Path;

use crate::json::Value;

/// Upper bound of the paired 95% interval of candidate/reference time.
#[derive(Clone, Copy, Debug)]
pub struct Runtime {
    /// The ceiling a case must stay at or below.
    pub ceiling: f64,
    /// The optimisation target.
    pub target: f64,
}

impl Runtime {
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join("harness/policy.json");
        let policy =
            crate::checks::read_json(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let field = |key: &str| {
            policy
                .get("runtime")
                .and_then(|r| r.get(key))
                .and_then(Value::as_f64)
                .ok_or_else(|| format!("{}: runtime without {key}", path.display()))
        };
        Ok(Runtime {
            ceiling: field("ratio_max")?,
            target: field("optimisation_target")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Runtime;

    #[test]
    fn the_repository_policy_is_the_published_runtime_gate() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let runtime = Runtime::load(&root).unwrap();
        assert_eq!((runtime.ceiling, runtime.target), (1.2, 0.8));
    }
}
