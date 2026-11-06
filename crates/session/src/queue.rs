//! Bounded access-unit queue. A full queue refuses the access unit and the
//! feeder keeps it (backpressure).
use std::collections::VecDeque;

/// Capacity of one lane's pending input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLimits {
    pub units: usize,
    pub bytes: usize,
    /// A single access unit larger than this is refused as invalid.
    pub unit_bytes: usize,
}

impl QueueLimits {
    /// About two seconds of high-rate video access units.
    pub const VIDEO: Self = Self {
        units: 120,
        bytes: 32 << 20,
        unit_bytes: 16 << 20,
    };
    /// About two seconds of audio frames at any selected codec rate.
    pub const AUDIO: Self = Self {
        units: 256,
        bytes: 4 << 20,
        unit_bytes: 1 << 20,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedError {
    /// The queue is full; retry the same access unit after output progresses.
    Backpressure,
    /// The access unit exceeds the per-unit limit or is empty.
    Invalid,
    /// No session accepts input (nothing loaded or the lane has no codec).
    Idle,
}

#[derive(Debug)]
pub(crate) struct Unit {
    pub data: Vec<u8>,
    pub pts: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct UnitQueue {
    limits: QueueLimits,
    units: VecDeque<Unit>,
    bytes: usize,
}

impl UnitQueue {
    pub fn new(limits: QueueLimits) -> Self {
        Self {
            limits,
            units: VecDeque::new(),
            bytes: 0,
        }
    }
    pub fn push(&mut self, data: &[u8], pts: Option<i64>) -> Result<(), FeedError> {
        if data.is_empty() || data.len() > self.limits.unit_bytes {
            return Err(FeedError::Invalid);
        }
        if self.units.len() >= self.limits.units || self.bytes + data.len() > self.limits.bytes {
            return Err(FeedError::Backpressure);
        }
        self.bytes += data.len();
        self.units.push_back(Unit {
            data: data.to_vec(),
            pts,
        });
        Ok(())
    }
    pub fn pop(&mut self) -> Option<Unit> {
        let unit = self.units.pop_front()?;
        self.bytes -= unit.data.len();
        Some(unit)
    }
    #[allow(clippy::option_option)] // `None`: empty queue; `Some(None)`: unit without a timestamp.
    pub fn front_pts(&self) -> Option<Option<i64>> {
        self.units.front().map(|unit| unit.pts)
    }
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.units.len()
    }
    pub fn clear(&mut self) {
        self.units.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_queue_refuses_without_dropping_and_recovers() {
        let mut queue = UnitQueue::new(QueueLimits {
            units: 2,
            bytes: 10,
            unit_bytes: 8,
        });
        assert_eq!(queue.push(&[], None), Err(FeedError::Invalid));
        assert_eq!(queue.push(&[0; 9], None), Err(FeedError::Invalid));
        queue.push(&[1; 4], Some(1)).unwrap();
        queue.push(&[2; 4], Some(2)).unwrap();
        assert_eq!(queue.push(&[3], Some(3)), Err(FeedError::Backpressure));
        assert_eq!(queue.pop().unwrap().pts, Some(1));
        assert_eq!(
            queue.push(&[3; 7], Some(3)),
            Err(FeedError::Backpressure),
            "byte bound"
        );
        queue.push(&[3; 6], Some(3)).unwrap();
        assert_eq!(queue.len(), 2);
        queue.clear();
        assert!(queue.is_empty());
        queue.push(&[4; 8], None).unwrap();
    }
}
