# MSA Core Architecture Migration

Working branch: `dev/msa-core-architecture`

Base checkpoint: `aa4468978202fc345535c875b29d936ea81e23e8`
from `dev/msa-solo-rebuild`.

## Goal

SAirplay2 must converge on one source of truth for AirPlay transport:

```
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

This branch does **not** merge the old native engine with MSA SOLO.

The old engine is a source of:
- discovery/catalog/UI integration;
- group-orchestration requirements;
- hardware evidence and acceptance tests.

The MSA-derived implementation is the only implementation source for:
- HAP / encrypted RTSP;
- PTP / NTP behavior;
- START / PAUSE / PLAY / FLUSH / STOP lifecycle;
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
with the validated checkpoint.

## Phase 2 — one Windows PCM hub

Replace mode-specific capture ownership with one producer:

```
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

Each migration phase must preserve or improve:
1. HomePod 16/44.1 realtime;
2. HomePod 24/48 opt-in realtime;
3. Naim buffered/realtime route selected by current evidence;
4. pause / long source-idle / resume;
5. clean Stop;
6. Stereo Pair;
7. MultiRoom member failure and rejoin;
8. AirPort explicit RAOP path.

CI PASS is not hardware PASS.

## Cleanup rule

Do not rename/reorganize the MSA transport internals merely to reduce file
count. First remove duplicate *responsibility*. File cleanup comes only after
hardware parity.
