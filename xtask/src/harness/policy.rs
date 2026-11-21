//! The gates of `harness/policy.json`. It is the only definition of the runtime ceiling
//! and target and of the memory ceilings: every runner and grader reads them from here,
//! and nothing restates them.

use std::path::Path;

use crate::json::{self, Value};

const FILE: &str = "harness/policy.json";

/// Upper bound of the paired 95% interval of candidate/reference time.
#[derive(Clone, Copy, Debug)]
pub struct Runtime {
    /// The ceiling a case must stay at or below.
    pub ceiling: f64,
    /// The optimisation target.
    pub target: f64,
}

/// Upper bounds of the paired 95% intervals of candidate/reference memory. A case
/// passes only strictly below each ceiling.
#[derive(Clone, Copy, Debug)]
pub struct Memory {
    /// The ceiling of the peak live heap ratio.
    pub heap: f64,
    /// The ceiling of the total resident peak RSS ratio.
    pub total_rss: f64,
}

/// Both gates.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub runtime: Runtime,
    pub memory: Memory,
}

impl Policy {
    pub fn load(root: &Path) -> Result<Self, String> {
        Ok(Policy {
            runtime: Runtime::load(root)?,
            memory: Memory::load(root)?,
        })
    }
}

impl Runtime {
    pub fn load(root: &Path) -> Result<Self, String> {
        let field = section(root, "runtime")?;
        Ok(Runtime {
            ceiling: field("ratio_max")?,
            target: field("optimisation_target")?,
        })
    }
}

impl Memory {
    pub fn load(root: &Path) -> Result<Self, String> {
        let field = section(root, "memory")?;
        Ok(Memory {
            heap: field("peak_live_heap_ratio_max_exclusive")?,
            total_rss: field("total_resident_peak_rss_ratio_max_exclusive")?,
        })
    }

    /// The ceiling a collection that stops early must have settled both ratios
    /// under: the stricter of the two.
    pub fn ceiling(self) -> f64 {
        self.heap.min(self.total_rss)
    }
}

/// Reads the numbers of one section of the policy.
fn section(
    root: &Path,
    name: &'static str,
) -> Result<impl Fn(&str) -> Result<f64, String>, String> {
    let path = root.join(FILE);
    let policy = json::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(move |key: &str| {
        policy
            .get(name)
            .and_then(|section| section.get(key))
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("{}: {name} without {key}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::{Memory, Runtime};

    #[test]
    fn the_repository_policy_has_a_target_below_its_ceiling() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let runtime = Runtime::load(&root).unwrap();
        assert!(0.0 < runtime.target && runtime.target < runtime.ceiling);
    }

    #[test]
    fn the_repository_policy_requires_memory_strictly_below_the_reference() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let memory = Memory::load(&root).unwrap();
        assert!(0.0 < memory.ceiling() && memory.heap <= 1.0 && memory.total_rss <= 1.0);
    }
}
