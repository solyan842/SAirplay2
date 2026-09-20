# SAirplay2 development rules

This file is a standing project rule.

## Source-first rule

Before implementing or changing any AirPlay/RAOP/HAP/PTP/RTSP/audio transport behavior:

1. Read the current upstream/reference source, not only summaries.
2. Cross-check the behavior against at least one independent implementation or protocol reference where available.
3. Record what is proven, what is inferred, and what remains unknown.
4. Do not copy a legacy SolYan behavior merely because it previously worked.
5. Do not patch around a protocol uncertainty. Resolve the protocol question first.
6. Add an invariant/unit test for every behavior that can be tested without hardware.
7. Run Windows CI after each coherent change.
8. Hardware observations outrank assumptions, but hardware-specific workarounds must remain isolated and documented.

## Primary references

- music-assistant/airplay-cli
  - DESIGN.md for architecture and behavior notes.
  - src/ap2_client.c for route, native session, realtime sender and pacing.
  - src/ap2_hap.c for HAP pairing and encrypted RTSP framing.
  - src/ap2_bplist.cpp / src/ap2_plist.c for keyed plist behavior.
  - src/ap2_ptp.c for timing behavior.
  - libraop for RAOP compatibility.

## Independent cross-checks

Use as appropriate:

- OwnTone AirPlay output implementation.
- pyatv AirPlay/HAP implementation and protocol documentation.
- HAP specification where publicly available/usable.
- crate/library upstream documentation for Rust dependencies.

## Evidence labels

Every non-trivial protocol design note should be mentally classified as:

- PROVEN-SOURCE: directly confirmed in current reference code.
- CROSS-CHECKED: confirmed independently by another implementation/spec.
- HARDWARE-MEASURED: observed on real receiver hardware.
- INFERENCE: plausible but not yet proven.
- UNKNOWN: do not implement as a protocol fact.

## Stability policy

The first stable target stays:

- ALAC
- 44.1 kHz
- 16-bit
- stereo
- 352 frames per packet

No 24-bit, 48 kHz, buffered type 103, multi-room, metadata/MRP or convenience
feature may delay or destabilize the first reliable single-speaker 16/44.1 path.
