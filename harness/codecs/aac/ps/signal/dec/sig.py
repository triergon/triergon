"""Core-rate (22.05 kHz) test signals for the decorrelator recovery."""
import numpy as np

FS = 22050.0
BW = 44100.0 / 128  # width of one 64-band QMF band in Hz


def length(nf):
    return 1024 * (nf + 1)


def tone(nf, f, amp=1.0, phase=0.0):
    n = np.arange(length(nf))
    return amp * np.cos(2 * np.pi * f / FS * n + phase)


def band_noise(nf, flo, fhi, rms=1.0, seed=1):
    """White noise restricted (FFT mask) to flo..fhi Hz, scaled to the given rms."""
    rng = np.random.default_rng(seed)
    L = length(nf)
    x = rng.standard_normal(L)
    X = np.fft.rfft(x)
    f = np.fft.rfftfreq(L, 1 / FS)
    X[(f < flo) | (f > fhi)] = 0
    y = np.fft.irfft(X, L)
    return y * rms / np.sqrt(np.mean(y ** 2))


def qmf_centre(k):
    return (k + 0.5) * BW
