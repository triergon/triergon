/// The work one step may do, in units the operation defines: a byte read, a
/// run decoded, a pixel written. Work is charged before it is done, and an
/// operation that cannot charge returns [`Step::Yield`](crate::Step::Yield), so
/// the time of a step is bounded whatever the input.
#[derive(Debug)]
pub struct WorkBudget {
    initial: u64,
    remaining: u64,
}

impl WorkBudget {
    /// A budget of `units`.
    pub fn new(units: u64) -> Self {
        Self {
            initial: units,
            remaining: units,
        }
    }
    /// Units left.
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
    /// Units charged so far.
    pub fn spent(&self) -> u64 {
        self.initial - self.remaining
    }
    /// Charges up to `maximum` units and returns how many: fewer, down to
    /// zero, when fewer are left.
    pub fn take(&mut self, maximum: u64) -> u64 {
        let units = self.remaining.min(maximum);
        self.remaining -= units;
        units
    }
    /// Charges exactly `units`, or nothing and returns `false` when fewer are
    /// left.
    pub fn try_charge(&mut self, units: u64) -> bool {
        if units > self.remaining {
            return false;
        }
        self.remaining -= units;
        true
    }
}
