#!/usr/bin/env python3
# Generate contrib/alerts/ding.wav — the alert chime used by mattyx-alerts.py.
#
# A short (~180 ms) pleasant two-tone "ding": an 880 Hz tone followed by a
# 1318 Hz tone (E6, a perfect fifth up), each with a fast attack and a smooth
# release so consecutive dings never click. Pure standard library (`wave`),
# no dependencies, no downloaded audio.
#
# Regenerate from the repo root with:
#   python3 contrib/alerts/make_ding.py
#
# The output is deterministic: re-running the script reproduces the same bytes,
# so the .wav can be regenerated and diffed like any other source file.
#
# Format: 44100 Hz, 16-bit signed little-endian PCM, mono. ~31 KB (< 50 KB).

import math
import os
import struct
import wave

SAMPLE_RATE = 44100
BIT_DEPTH = 16
CHANNELS = 1

# (frequency Hz, onset sec, duration sec)
TONES = [
    (880.0, 0.000, 0.090),
    (1318.0, 0.085, 0.095),  # starts before the first ends → tiny overlap
]
TOTAL_SECONDS = 0.180

ATTACK_SECONDS = 0.004   # fast, click-free onset
RELEASE_SECONDS = 0.030  # smooth fade to silence at the end of each tone
PEAK = 0.80              # headroom below full scale


def _envelope(t: float, duration: float) -> float:
    """Attack/sustain/release envelope in [0, 1] for one tone."""
    if t < 0.0 or t > duration:
        return 0.0
    env = 1.0
    if t < ATTACK_SECONDS:
        env *= t / ATTACK_SECONDS
    release_start = duration - RELEASE_SECONDS
    if t > release_start:
        # Raised-cosine (equal-power-ish) fade-out avoids a click.
        x = (t - release_start) / RELEASE_SECONDS
        env *= 0.5 * (1.0 + math.cos(math.pi * min(x, 1.0)))
    return env


def _build_samples():
    total = int(round(TOTAL_SECONDS * SAMPLE_RATE))
    samples = [0.0] * total
    for freq, onset, duration in TONES:
        start = int(round(onset * SAMPLE_RATE))
        count = int(round(duration * SAMPLE_RATE))
        omega = 2.0 * math.pi * freq / SAMPLE_RATE
        for i in range(count):
            idx = start + i
            if idx >= total:
                break
            t = i / SAMPLE_RATE
            samples[idx] += _envelope(t, duration) * math.sin(omega * i)
    return samples


def main() -> None:
    samples = _build_samples()
    peak = max(abs(s) for s in samples) or 1.0
    scale = PEAK * 32767.0 / peak
    frames = b"".join(
        struct.pack("<h", max(-32768, min(32767, int(round(s * scale)))))
        for s in samples
    )

    out_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ding.wav")
    with wave.open(out_path, "wb") as wav:
        wav.setnchannels(CHANNELS)
        wav.setsampwidth(BIT_DEPTH // 8)
        wav.setframerate(SAMPLE_RATE)
        wav.writeframes(frames)

    print("wrote %s (%d bytes, %.0f ms)" % (
        out_path, os.path.getsize(out_path), TOTAL_SECONDS * 1000.0))


if __name__ == "__main__":
    main()
