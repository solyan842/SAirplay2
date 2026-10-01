# MSA SOLO Time-Domain Parity Invariant

This document is an architecture lock for the independent MSA SOLO engine.

## Pinned source of truth

- music-assistant/airplay-cli: `431c5c582eef9307c4e39c50a0ea65e970bc1128`
- its libraop submodule: `81c2182649da8645ac2a58b78e9f370c79a4165b`

Pinned libraop `raopcl_get_ntp(NULL)` packs `gettime_us()` directly as
`seconds << 32 | fraction`. MSA AP2 scheduling therefore uses the system/Unix
wall-clock fixed-point domain. It does **not** add the RFC/NTP 1900 epoch delta.

The AirPlay NTP timing responder is a different protocol boundary and does use
RFC/NTP epoch 1900. The difference is exactly 2,208,988,800 seconds.

## Locked domains

1. `SourceNtp`: MSA/libraop AP2 scheduling, pacing, START, realtime sync and
   buffered remaining-lead calculations.
2. `RfcNtp`: AirPlay NTP responder packets only.
3. PTP master time: nanoseconds, named `*_ns`; it enters AP2 only at the
   explicit PTP anchor boundary.
4. Monotonic time: Rust `Instant` / elapsed microseconds for local pacing and
   timeout decisions only.
5. RTP/audio frame domain: frame counts and wrapping RTP timestamps; conversion
   from wall time is only through `SourceNtp`.

## Invariants

- No generic `system_time_to_ntp` helper is exported.
- RFC-NTP cannot be passed to core AP2 scheduling without an explicit raw escape.
- `ClockFloor`, runtime `start_ntp`, realtime sync timing and buffered START
  use `SourceNtp`.
- GUI immediate START fails closed when accepted time differs by more than
  10 seconds. Normal PTP seating is below this bound; an epoch mix is not.
- Regression tests must preserve the exact 2,208,988,800-second separation.
- Hardware PASS still requires audible output; CONNECT/READY/START alone are
  transport evidence, not audible proof.

## Hardware gate

Before 24/48, Stereo Pair or MultiRoom work continues:

1. Black, native AP2, 44.1 kHz / 16-bit.
2. CONNECT -> READY.
3. START accepted within the immediate-start bound.
4. Timeline diagnostic has a plausible pacing-ahead value.
5. Audio counters advance and sound is audibly present.
6. STOP/START once more, then retain the GUI log.


## Hardening against recurrence

- Raw fixed-point constructors/accessors are crate-private escape hatches.
- The raw fixed-point helpers in `native_timeline.rs` are private to that module.
- The RAOP lane also carries absolute scheduling instants as `SourceNtp`, so
  native AP2 and RAOP cannot silently diverge on epoch semantics.
- Any future code that needs RFC/NTP must explicitly use `RfcNtp`; scheduling
  APIs do not accept it.
