# MSA Core Architecture Migration

Working branch: `dev/msa-core-architecture`

Base checkpoint: `aa4468978202fc345535c875b29d936ea81e23e8`
from `dev/msa-solo-rebuild`.

Authoritative current architecture/state lock:
`docs/MSA-CORE-SOURCE-LOCK.md`.

If an older SOLO/PROJECT-STATE document conflicts with the current MSA Core lock,
that conflicting section is historical until re-verified against pinned MSA source
and current hardware evidence.

## Goal

SAirplay2 must converge on one source of truth for AirPlay transport:

```text
Discovery + Capability
        |
Route Policy
        |
Session Coordinator
  Single / Pair / MultiRoom
        |
MSA Receiver Core
 control / timing / media
        |
Windows PCM Hub
```

Legacy AirPlay 1 / RAOP stays a separate pinned-libraop adapter.

## Non-negotiable migration rule

This branch does **not** merge the old native engine with MSA SOLO/Core.

The old engine is a source of:
- discovery/catalog/UI integration;
- group-orchestration requirements;
- hardware evidence and acceptance tests.

The MSA-derived implementation is the only implementation source for:
- HAP / encrypted RTSP;
- PTP / NTP behavior;
- START / PAUSE / PLAY / FLUSH / STANDBY / STOP lifecycle;
- realtime type 96 and buffered type 103;
- ALAC / crypto;
- pacing / recovery;
- RTX / feedback;
- metadata / MRP transport semantics.

Windows-specific code may adapt capture and lifecycle signals to that contract
but may not invent a second transport contract.

## Phase 1 — receiver facade

Introduce a neutral receiver-level facade over the hardware-validated SOLO
transport. Single playback uses this facade first.

**Behavioral requirement:** zero wire change. Diagnostics may retain the
`MSA SOLO` wording during migration so new builds can be compared directly
with validated checkpoints.

## Phase 2 — one Windows PCM hub

Replace mode-specific capture ownership with one producer:

```text
WASAPI shared capture
      |
format normalization
      |
bounded PCM hub
   /     |     \
receiver receiver receiver
```

The producer must never block on AirPlay network I/O. SILENT PCM remains valid
PCM. Capture absence is an adapter signal, not protocol EOF.

### Current validated Buffered idle/resume adapter

Real Naim Mu-so Qb 16/44.1 hardware testing on 2026-10-03 established the
current field-proven Windows adapter path:

```text
capture idle >= 500 ms
    -> MSA STANDBY + FLUSHBUFFERED
    -> flush pre-boundary local PCM
    -> wait for fresh post-flush non-SILENT PCM
    -> deferred START
    -> anchor using max(400 ms, receiver effective lead)
```

Baseline commit: `57756a04d58b66a1732fd87badacf06aea31e8fb`.

The tested Naim negotiated ~2000 ms lead and resumed at ~1.99 s pacing headroom
on repeated cycles, with smooth audible playback. Do not replace this with the
failed inferred in-place rate-0/rate-1 resume path.

CI regression locks were added in:
- `0ae8270984a92fbe607c23fdf6e8ff0261198374`;
- `b3e895f37071cbd34c126b216ee0182de1f7fcee`.

## Active consolidation step

Before adding more transport behavior, migrate the already validated
`windows_audio_worker.rs` runtime replacements out of
`scripts/apply-validated-branch-fixes.ps1` and into committed Rust source.

This consolidation must be **zero behavior change**. It exists only so:
- GitHub source;
- local builds;
- CI builds;
- field-tested artifacts

all execute the same code without a hidden runtime source rewrite.

Unrelated GUI runtime replacements may remain temporarily and are migrated
separately.

## Phase 3 — coordinator-owned grouping

Stereo Pair and MultiRoom become coordination layers over receiver sessions.

Coordinator owns only:
- membership;
- one shared capture source;
- shared group timing/START planning;
- fan-out;
- join/remove/recovery;
- group-level format planning.

Receiver Core continues to own every receiver's protocol lifecycle and media
transport. No group-specific RTP/ALAC/PTP implementation is allowed.

## Phase 4 — retire duplicate native transport

Only after Single, Pair, MultiRoom and hardware gates pass on MSA Core:
- remove old native Single sender;
- remove old native Pair/MultiRoom transport duplication;
- keep discovery/catalog/GUI pieces that remain useful;
- keep legacy RAOP isolated.

## Hardware gates

Current order is intentional:

1. Naim Buffered 16/44.1 lifecycle regression around the validated baseline;
2. HomePod 16/44.1 regression;
3. Apple TV 16/44.1 regression;
4. AirPort explicit RAOP path kept separate;
5. lock the 16-bit matrix;
6. then resume 24-bit work;
7. then Stereo Pair;
8. then MultiRoom.

Broader migration must preserve or improve:
- HomePod 16/44.1 realtime;
- HomePod 24/48 opt-in realtime when that phase resumes;
- Naim route/lifecycle selected by current evidence;
- pause / long source-idle / resume;
- clean Stop;
- Stereo Pair;
- MultiRoom member failure and rejoin;
- AirPort explicit RAOP path.

CI PASS is not hardware PASS.

## Cleanup rule

Do not rename/reorganize the MSA transport internals merely to reduce file
count. First remove duplicate *responsibility*. File cleanup comes only after
hardware parity.

Do not move to 24-bit merely because one 16-bit device is now stable. First
finish source consolidation and cross-device 16-bit regression so the MSA Core
baseline cannot silently drift.
