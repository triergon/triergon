"""Owner-approved total-resident-peak RSS arithmetic for initial delivery.

This module does not admit measurements. Callers must first validate complete
work, all observer receipts, calibration, provenance and isolation as before.
Frozen collectors and their growth-based verdicts remain unchanged.
"""
from dataclasses import dataclass
import math

from .runner import ratio_interval

POLICY = "initial-delivery-total-resident-rss-v2"


@dataclass(frozen=True)
class ResidentPeakPair:
    candidate_upper_bytes: int
    reference_observed_bytes: int
    page_size: int


def total_resident_rss_gate(pairs, *, seed=280):
    """Grade exactly 30 previously admitted pairs with the original +2/-2 pages.

    The numerator uses a proven candidate ceiling and the denominator an
    observed reference peak. Neither side uses starting RSS or reference VmPeak.
    Passing this arithmetic gate alone never qualifies an operation or module.
    """
    if len(pairs) != 30:
        raise ValueError("total RSS requires all 30 paired observations")
    bounded = []
    for pair in pairs:
        if not isinstance(pair, ResidentPeakPair):
            raise ValueError("explicit resident peak pair required")
        values = (pair.candidate_upper_bytes, pair.reference_observed_bytes, pair.page_size)
        if any(type(value) is not int or not 0 < value <= (1 << 64) - 1 for value in values):
            raise ValueError("positive integer resident bytes/page size required")
        page = pair.page_size
        if page & (page - 1):
            raise ValueError("page size must be a power of two")
        upper = pair.candidate_upper_bytes + 2 * page
        lower = pair.reference_observed_bytes - 2 * page
        if lower <= 0:
            raise ValueError("reference total peak does not exceed the allowance")
        bounded.append((upper, lower))
    interval = ratio_interval(bounded, seed=seed)
    if any(type(value) not in (int, float) or not math.isfinite(value) or value < 0
           for value in interval.values()):
        raise ValueError("non-finite total RSS confidence interval")
    return {
        "policy": POLICY,
        "pairs": 30,
        "seed": seed,
        "bounded_peak_pairs": bounded,
        "ratio": interval,
        "lower_total_resident_RSS": interval["upper_95"] < 1.0,
        "scope": "arithmetic over prevalidated observations; not measurement admission",
    }
