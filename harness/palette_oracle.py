"""Inverse ITU-R BT.601-7 and BT.709-6 matrices with palette quantization."""
from fractions import Fraction as Q
from math import floor


def studio_matrix(name):
    kr, kb = {
        "bt601": (Q("0.299"), Q("0.114")),
        "bt709": (Q("0.2126"), Q("0.0722")),
    }[name]
    kg = 1 - kr - kb
    red = 2 * (1 - kr)
    blue = 2 * (1 - kb)
    chroma = Q(255, 224)
    luma = Q(255, 219)
    # Multiply [Y'-16, Cr-128, Cb-128] by the inverse matrix after
    # quantizing each coefficient to the nearest 1/1024.
    return tuple(
        tuple(floor(abs(c) * 1024 + Q(1, 2)) * (-1 if c < 0 else 1) for c in row)
        for row in (
            (luma, chroma * red, 0),
            (luma, -chroma * kr * red / kg, -chroma * kb * blue / kg),
            (luma, 0, chroma * blue),
        )
    )


def trace(path, matrix, rounding):
    coefficients = studio_matrix(matrix)
    bias = {"nearest": 512, "half-down": -512}[rounding]
    rows = []
    data = path.read_bytes()
    for offset in range(0, len(data), 4):
        y, cr, cb, alpha = data[offset:offset + 4]
        signal = y - 16, cr - 128, cb - 128
        channels = [
            max(0, min(255, (sum(c * v for c, v in zip(component, signal)) + bias) // 1024))
            for component in coefficients
        ]
        rgb = int.from_bytes(bytes(channels), "big")
        rows.append(f"3 {y} {cr} {cb} {alpha} {rgb} {(alpha << 24) | rgb}")
    return rows
