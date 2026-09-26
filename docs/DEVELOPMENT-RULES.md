# SAirplay2 development rules

This file is a standing project rule for the active development branch.

## Source-first rule

Before changing any AirPlay/RAOP/HAP/PTP/RTSP/audio transport behavior:

1. Read the pinned upstream/reference source, not only summaries.
2. Cross-check current Music Assistant behavior when the server layer owns the
   lifecycle decision.
3. Use real-device logs to decide whether a source-aligned mechanism is actually
   failing on hardware.
4. Separate PROVEN-SOURCE, CROSS-CHECKED, HARDWARE-MEASURED, INFERENCE and
   UNKNOWN conclusions.
5. Do not add a protocol workaround for an UNKNOWN.
6. Keep platform substitutions below the wire/lifecycle contract.
7. Add or retain invariant tests for behavior that does not require hardware.
8. Run Windows CI after every coherent code change.
9. Never call a physical path PASS without a real-device log or explicit user
   observation.

## Primary references

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`
  - `src/ap2_client.c`
  - `src/ap2_ptp.c`
  - `src/ap2_hap.c`
  - `src/ap2_io.c`
  - `DESIGN.md`
- Music Assistant AirPlay provider:
  - project-pinned server snapshot;
  - current stable branch for lifecycle/START/late-join cross-checks.
- `philippe44/libraop@dadcfcaa26d988cdd3e3501ddf8286c224f1b494`
  for legacy RAOP behavior.

Independent references such as OwnTone, pyatv and public protocol documentation
may be used to cross-check, but do not override a directly measured/source-proven
contract without evidence.

## Branch policy

- `stable-1` at
  `a7cb24b1faa6b54abf7d24812b732ed08eb72524` is immutable.
- Active protocol/audio work lives on `dev/hires-source-port`.
- Old `dev/msa-*`, diagnostic and one-off experiment branches are historical
  evidence only once their behavior has been absorbed or superseded.
- Do not merge an old experiment branch wholesale just because Git reports it
  as diverged. Compare its actual behavior first.
- In particular, never reintroduce an older synthetic-EOF workaround after the
  later MSA-aligned zero-read semantics superseded it.

## Locked audio/timing invariants

Do not change without fresh source or hardware evidence:

- 352 PCM frames per native packet;
- realtime payload type 96 and buffered type 103 remain separate lanes;
- digital-zero PCM is valid audio;
- `frames == 0` from Windows loopback is not EOF;
- one shared PTP timeline per group;
- Apple/HomePod is not auto-forced to buffered type 103;
- solo hard peer/control close is terminal;
- group member failure isolates the member and preserves survivors;
- realtime RTX retains the exact encrypted wire packet;
- feedback cadence/timeout stays source-aligned;
- START uses clock readiness and verified committed instants.

Current START constants:

- solo base lead: 400 ms;
- clock-readiness timeout: 2500 ms;
- readiness margin: 500 ms;
- cold group floor: 2500 ms;
- group convergence margin: 150 ms;
- convergence rounds: at most 4.

## Change discipline

Prefer the smallest patch that fixes a demonstrated discrepancy.

Do not:

- reduce 352 frames to fit MTU;
- force Apple receivers onto buffered type 103;
- invent per-member PTP for a group;
- treat silence amplitude as track/EOF state;
- synthesize EOF from prolonged Windows silence;
- raise a timeout repeatedly when a queue/pacing bug may be preventing progress;
- replace a working source-aligned path with device-name heuristics.

Hardware-specific exceptions must be narrow and documented. The current example
is `Mu-so Qb` buffered denial: the model remains native AirPlay 2 but avoids
automatic type 103 after physical testing showed type-103 rendering instability.

## Diagnostics policy

Keep diagnostics that can discriminate a protocol failure:

- PTP readiness/probe health;
- requested and committed START;
- WASAPI discontinuity count;
- RTP/media delivery anomaly;
- RTX requested/answered/expired/max wire size;
- group member isolation/rejoin;
- timeline head/pad debt at a low cadence.

Remove diagnostics that only classify PCM amplitude or dump large repetitive
packet sequences after the underlying issue has been resolved.

## Validation order

For a protocol/audio change:

1. source comparison;
2. static/unit/invariant tests;
3. GitHub Actions PASS;
4. focused physical test;
5. only then update the validated project state.

A CI PASS proves build/invariants, not speaker behavior. A hardware PASS applies
only to the exact tested path and format.

## Cleanup rule

Cleanup must not be a disguised refactor of working transport behavior.

Safe cleanup includes:

- dead fields/functions with no production caller;
- diagnostics whose premise is no longer part of the active state model;
- accidental files;
- stale documentation;
- duplicate helper logic that can drift from the canonical implementation.

Do not delete locked stable history or destructively rewrite old experiment
branches merely to reduce branch count.
