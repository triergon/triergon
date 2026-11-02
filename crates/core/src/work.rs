/// Per-step deterministic work. Format decoders charge bytes, runs, pixels and
/// other bounded inner-loop work before doing it; an exhausted step yields.
#[derive(Debug)]
pub struct WorkBudget {
    initial: u64,
    remaining: u64,
}

impl WorkBudget {
    pub fn new(units: u64) -> Self {
        Self {
            initial: units,
            remaining: units,
        }
    }
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
    pub fn spent(&self) -> u64 {
        self.initial - self.remaining
    }
    pub fn take(&mut self, maximum: u64) -> u64 {
        let units = self.remaining.min(maximum);
        self.remaining -= units;
        units
    }
    pub fn try_charge(&mut self, units: u64) -> bool {
        if units > self.remaining {
            return false;
        }
        self.remaining -= units;
        true
    }
}
