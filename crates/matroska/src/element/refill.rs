//! Header and discard refills: the exact request and the bounded expanded request
//! a blocking adapter may issue for an ordinary header or a skipped body.
use super::{ElementReader, Phase};
use crate::{Error, Result};
use triergon_core::InputRequest;

/// Format-authorized ordinary-header refill. Callers cannot construct an
/// expanded request by widening an already issued canonical hint themselves.
#[derive(Debug)]
pub(crate) struct HeaderRefillRequest {
    exact: InputRequest,
    expanded: InputRequest,
}
impl HeaderRefillRequest {
    pub(crate) fn exact(&self) -> &InputRequest {
        &self.exact
    }
    pub(crate) fn expanded(&self) -> &InputRequest {
        &self.expanded
    }
    pub(crate) fn matches_exact(&self, request: &InputRequest) -> bool {
        same_header_request(&self.exact, request)
    }
    pub(crate) fn matches_expanded(&self, request: &InputRequest) -> bool {
        same_header_request(&self.expanded, request)
    }
}
fn same_header_request(a: &InputRequest, b: &InputRequest) -> bool {
    a.source == b.source && a.token == b.token && a.offset == b.offset && a.maximum == b.maximum
}
impl ElementReader {
    pub(crate) fn exact_header_request(&self) -> Option<InputRequest> {
        let Phase::Header(state) = &self.phase else {
            return None;
        };
        if state.have >= state.target || self.failed {
            return None;
        }
        let mut request = self.input.request();
        request.maximum = request.maximum.min(state.target - state.have);
        Some(request)
    }
    pub(crate) fn bounded_header_refill(
        &self,
        exact: &InputRequest,
        maximum: usize,
    ) -> Result<Option<HeaderRefillRequest>> {
        let Phase::Header(state) = &self.phase else {
            return Ok(None);
        };
        if state.have >= state.target || self.failed || maximum == 0 {
            return Ok(None);
        }
        if self.seen_segment && self.segment.is_none() {
            return Ok(None);
        }
        let core = self.input.request();
        if exact.source != core.source
            || exact.token != core.token
            || exact.offset != core.offset
            || exact.maximum != core.maximum.min(state.target - state.have)
        {
            return Err(Error::Invalid("header refill request identity changed"));
        }
        // Zero is the existing real EOF-probe request, never a manufactured
        // parent/scan EOF. Narrow/malformed-edge canonical hints stay exact.
        if exact.maximum == 0 {
            return Ok(None);
        }
        let cap = core.maximum.min(maximum).min(4096);
        self.expanded_refill(exact, core, cap)
    }
    /// A discarded body's exact request may be widened in the same bounded way
    /// as a header refill, so the following header needs no separate supply.
    /// Discarded bytes never become a payload owner. Only an immutable source,
    /// whose views need no new storage, uses this (see the blocking adapter).
    pub(crate) fn bounded_discard_refill(
        &self,
        exact: &InputRequest,
        maximum: usize,
    ) -> Result<Option<HeaderRefillRequest>> {
        let Phase::Body(body) = &self.phase else {
            return Ok(None);
        };
        if body.collect || body.remaining == 0 || self.failed || maximum == 0 {
            return Ok(None);
        }
        if self.seen_segment && self.segment.is_none() {
            return Ok(None);
        }
        let core = self.input.request();
        let body_maximum =
            usize::try_from(body.remaining.min(8192)).map_err(|_| Error::Overflow)?;
        if exact.source != core.source
            || exact.token != core.token
            || exact.offset != core.offset
            || exact.maximum != core.maximum.min(body_maximum)
        {
            return Err(Error::Invalid("discard refill request identity changed"));
        }
        if exact.maximum == 0 {
            return Ok(None);
        }
        let cap = core.maximum.min(maximum);
        self.expanded_refill(exact, core, cap)
    }
    fn expanded_refill(
        &self,
        exact: &InputRequest,
        core: InputRequest,
        cap: usize,
    ) -> Result<Option<HeaderRefillRequest>> {
        let mut cap = cap.min(usize::try_from(self.scan_left).unwrap_or(usize::MAX));
        for end in [
            self.segment.and_then(|m| m.end),
            self.cluster.and_then(|m| m.end),
        ]
        .into_iter()
        .flatten()
        {
            cap = cap.min(usize::try_from(end.saturating_sub(core.offset)).unwrap_or(usize::MAX));
        }
        if cap <= exact.maximum {
            return Ok(None);
        }
        let mut expanded = core;
        expanded.maximum = cap;
        Ok(Some(HeaderRefillRequest {
            exact: exact.clone(),
            expanded,
        }))
    }
}
