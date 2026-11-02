"""Preserve first clean results and terminal findings on an unchanged operation.

Callers first bind worker, contract, input and operation identities. Returned
records are retained verbatim; None permits a new observation. Old reports stay
immutable even when contamination or a changed observer permits collection.
"""


def _complete(measurement, pairs, minimum_pairs, iterations=None):
    same = measurement.get("pairs") == pairs
    if iterations is not None:
        same &= measurement.get("iterations") == iterations
    if same:
        return measurement
    old_pairs = measurement.get("pairs")
    if type(old_pairs) is int and 0 < old_pairs < minimum_pairs <= pairs:
        # A short, explicitly diagnostic screen is not a complete gate run.
        return None
    raise ValueError("completed clean measurement cannot be replaced by changing collection settings; change the workers before requalification")


def retained_timing(measurement, pairs, iterations, minimum_pairs):
    if not measurement or measurement.get("status") == "contaminated":
        return None
    if measurement.get("status") != "measured":
        return measurement
    return _complete(measurement, pairs, minimum_pairs, iterations)


def retained_memory(measurement, pairs, minimum_pairs, previous_instrumentation, instrumentation):
    if not measurement or measurement.get("status") == "contaminated":
        return None
    keys = ("probe_sha256", "observer_sha256")
    if (previous_instrumentation and instrumentation
            and all(previous_instrumentation.get(k) and instrumentation.get(k) for k in keys)
            and any(previous_instrumentation[k] != instrumentation[k] for k in keys)):
        return None
    if measurement.get("status") != "qualified":
        return measurement
    return _complete(measurement, pairs, minimum_pairs)
