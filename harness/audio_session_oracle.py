"""Expected byte-exact session output for a caller's three PCM units."""


def trace(path, mode):
    units = path.read_bytes()
    if len(units) != 15 or list(units[::5]) != [1, 1, 2]:
        raise ValueError("expected three four-sample units")
    absent = (1 << 64) - 1
    if mode == "lifecycle":
        # First unit is dropped in full. The second loses two carried start
        # samples and one terminal sample. Rate change goes into its own block.
        return [
            f"1 2 1000 22000000 1 {units[8:9].hex()}",
            f"2 0 2000 {absent} 4 {units[11:].hex()}",
            f"3 0 2000 {absent} 2 fbfc",
            "4 3",
        ]
    if mode == "refusals":
        return [
            "5 0 1 1", "7 0 1 2",
            f"6 2 1000 10000000 4 {units[1:5].hex()}",
            "5 1 2 2", "7 1 1 2", "8 1 1",
        ]
    raise ValueError("unknown lifecycle control")
