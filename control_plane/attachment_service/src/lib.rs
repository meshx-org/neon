use utils::seqwait::MonotonicCounter;

mod compute_hook;
pub mod http;
mod node;
mod reconciler;
mod scheduler;
pub mod service;
mod tenant_state;

#[derive(Clone)]
enum PlacementPolicy {
    /// Cheapest way to attach a tenant: just one pageserver, no secondary
    Single,
    /// Production-ready way to attach a tenant: one attached pageserver and
    /// some number of secondaries.
    Double(usize),
}

#[derive(Ord, PartialOrd, Eq, PartialEq, Copy, Clone)]
struct Sequence(u64);

impl std::fmt::Display for Sequence {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl MonotonicCounter<Sequence> for Sequence {
    fn cnt_advance(&mut self, v: Sequence) {
        assert!(*self <= v);
        *self = v;
    }
    fn cnt_value(&self) -> Sequence {
        *self
    }
}

impl Sequence {
    fn next(&self) -> Sequence {
        Sequence(self.0 + 1)
    }
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        PlacementPolicy::Double(1)
    }
}
