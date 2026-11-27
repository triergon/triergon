"""Account for live allocations and work according to the public core contracts."""


def budget_trace(path):
    header, *steps = path.read_text(encoding="ascii").splitlines()
    kind, limit = header.split()
    limit = int(limit)
    if kind == "bare":
        if limit != 0 or steps:
            raise ValueError("bare case must test a zero-byte pool")
        return ["8 1"]
    if kind == "work":
        rows = []
        spent = 0
        for step in steps:
            op, amount = step.split()
            amount = int(amount)
            remaining = limit - spent
            if op == "charge":
                result = min(remaining, amount)
                spent += result
            elif op == "try_charge":
                result = int(amount <= remaining)
                spent += amount * result
            else:
                raise ValueError(f"unknown work operation: {op}")
            rows.append(f"3 {result} {spent} {limit - spent}")
        return rows

    # Bytes handles share a charge identified by the allocation's creation step.
    # Reservations and vectors each own their charge.
    rows = [f"3 1 0 {limit}"]
    handles = {}
    for serial, step in enumerate(steps):
        op, name, *args = step.split()
        charged = sum({item[0]: item[2] for item in handles.values()}.values())
        result = 0
        requested = 0
        after = handles.copy()
        if op in ("reserve", "bytes", "vector"):
            size = int(args[0]) if args else 0
            requested = size
            after[name] = (serial, op, size, size)
        elif op == "release":
            del after[name]
        elif op == "split":
            source, start, end = args
            allocation, form, capacity, length = handles[source]
            start, end = int(start), int(end)
            if 0 <= start <= end <= length:
                after[name] = (allocation, form, capacity, end - start)
            else:
                result = 3
        elif op == "clone":
            allocation, form, capacity, length = handles[args[0]]
            if form == "vector":
                requested = length
                after[name] = (serial, form, length, length)
            else:
                after[name] = (allocation, form, capacity, length)
        elif op in ("resize", "grow", "fill", "freeze"):
            allocation, form, capacity, length = handles[name]
            if op == "resize":
                size = int(args[0])
                requested = max(0, size - capacity)
                after[name] = (allocation, form, size, size)
            elif op == "freeze":
                after[name] = (allocation, "bytes", capacity, length)
            else:
                target = length + int(args[0])
                new_capacity = max(capacity, target)
                requested = new_capacity if new_capacity > capacity else 0
                after[name] = (allocation, form, new_capacity, target if op == "fill" else length)
        else:
            raise ValueError(f"unknown storage operation: {op}")
        if requested > limit:
            result = 1
        elif requested > limit - charged:
            result = 2
        if result == 0:
            handles = after
            charged = sum({item[0]: item[2] for item in handles.values()}.values())
        rows.append(f"3 {result} {charged} {limit - charged}")
    return rows
