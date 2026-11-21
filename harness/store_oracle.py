"""Whole-script oracle for the font store's budgeted LRU policy."""

from collections import OrderedDict


def store_trace(path, entry_bytes):
    overhead = int(entry_bytes)
    header, *commands = path.read_text(encoding="ascii").splitlines()
    tag, slots, extra = header.split()
    if tag != "budget" or overhead <= 0:
        raise ValueError("expected budget SLOTS BYTES and a positive entry charge")
    capacity = int(slots) * overhead + int(extra)
    # Oldest access first. The map holds only admitted values.
    entries = OrderedDict()
    held = set()
    rows = []
    for line in commands:
        words = line.split()
        if not words:
            continue
        verb, key, *args = words
        key = int(key)
        victims = []
        if verb == "get" and len(args) == 1:
            if key in entries:
                entries.move_to_end(key)
                outcome = "hit"
            else:
                size = int(args[0])
                outcome = "built-unkept"
                eligible = [victim for victim in entries if victim not in held]
                for victim in [*eligible, None]:
                    if sum(entries.values()) + size + (len(entries) + 1) * overhead <= capacity:
                        entries[key] = size
                        outcome = "built-kept"
                        break
                    if victim is not None:
                        del entries[victim]
                        victims.append(victim)
        elif verb == "hold" and not args:
            outcome = "missing"
            if key in entries:
                held.add(key)
                entries.move_to_end(key)
                outcome = "held"
        elif verb == "release" and not args:
            outcome = "unheld"
            if key in held:
                held.remove(key)
                outcome = "released"
        else:
            raise ValueError(f"invalid store access: {line}")
        row = (3, verb, key, outcome, len(victims), *victims, len(entries), sum(entries.values()))
        rows.append(" ".join(map(str, row)))
    return rows
