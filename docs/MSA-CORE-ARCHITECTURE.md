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

SAirplay2 converges on one receiver architecture and one application process while
keeping the protocol contracts separate exactly as pinned Music Assistant does:

```text
Discovery + Capability
        |
Route Policy
        |
Session Coordinator
 Single / Stereo Pair / MultiRoom
        |
MSA Receiver Core
   |                         |
   +-- Native AP2 Core       +-- RAOP Adapter
   |   HAP / encrypted RTSP  |   pinned libraop
   |   PTP / NTP             |   RAOP / AirPlay 1
   |   Type 96 / Type 103    |   AP2 RAOP-compat
   |   RTP / RTX / pacing    |   NTP / RAOP lifecycle
   |   ALAC / crypto         |
   |                         |
   +------------+------------+
                |
        Windows PCM Hub
```

**Binary rule:** Native AP2 and RAOP are separate transport contracts inside the
same `SAirplay2.exe`. Protocol separation does not imply a separate helper process.
The target RAOP implementation is pinned libraop linked in-process through a thin
C ABI/FFI boundary. `cliraop.exe` and `cliraop-msa-solo.exe` are historical CLI
wrappers, not part of the target runtime architecture.

This follows pinned `music-assistant/airplay-cli`: one unified owner resolves the
route, then dispatches to native AP2 or libraop-backed RAOP. SAirplay2 must preserve
that split instead of treating RAOP as an old/legacy engine.

## Naming lock

Use these names for new MSA Core work:

- **MSA Receiver Core** — common receiver/session facade and route ownership.
- **Native AP2 Core** — native AirPlay 2 transport implementation.
- **RAOP Adapter** — AirPlay 1 / RAOP and MSA-selected AirPlay 2 RAOP-compat transport.
- **Session Coordinator** — Single / Stereo Pair / MultiRoom orchestration only.
- **Windows PCM Hub** — Windows capture, format conversion/resampling and bounded PCM fan-out.

`LegacyGroupSession`, `LegacyMemberConfig` and `LegacyVolumeControl` are historical
names in the old integration. When their responsibility is migrated, use
`RaopGroupSession`, `RaopMemberConfig` and `RaopVolumeControl`. Do not perform a
cosmetic rename before responsibility has actually moved.

## Source-of-truth boundary

Pinned references remain the authority:

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`
- `music-assistant/server@f09136859e240fc7859160e186c2e2186e917715`
- MSA libraop submodule `81c2182649da8645ac2a58b78e9f370c79a4165b`

The comparison/current libraop pin is not allowed to replace the MSA pin in the
new RAOP Adapter.

The MSA-derived implementation owns:

- route resolution between Native AP2, RAOP and AP2 RAOP-compat;
- HAP / encrypted RTSP;
- PTP / NTP behavior;
- START / PAUSE / PLAY / FLUSH / STANDBY / STOP lifecycle;
- realtime Type-96 and buffered Type-103;
- RAOP/libraop timing and lifecycle when the route is RAOP;
- ALAC / crypto;
- pacing / recovery;
- RTX / feedback;
- metadata / transport semantics.

Windows-specific code may adapt capture and lifecycle signals to that contract,
but it must not invent a second AirPlay protocol contract. RAOP's private clock
and lifecycle must never leak into Native AP2.

## Phase 1 — receiver facade

A neutral receiver-level facade owns one route decision and dispatches to the
appropriate transport. Single playback migrates first.

The current `WindowsMsaSoloClient` already has the correct shape: one input config
resolves to `AirPlay2Native`, `Raop` or `AirPlay2Compat`. The remaining RAOP debt
is below that facade: the Windows RAOP session still spawns a helper process.

**Current implementation task:** replace that helper backend with an in-process,
64-bit, static pinned-libraop adapter without changing the route/lifecycle contract.
RAOP-only Single receivers must enter the same MSA Receiver facade even when they
advertise only `_raop._tcp` and no `_airplay._tcp` service.

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

### Field-proven Buffered idle/resume adapter

Real Naim Mu-so Qb 16/44.1 hardware testing established the field-proven Windows
adapter path:

```text
capture idle >= 500 ms
    -> MSA STANDBY + FLUSHBUFFERED
    -> flush pre-boundary local PCM
    -> wait for fresh post-flush non-SILENT PCM
    -> deferred START
    -> anchor using max(400 ms, receiver effective lead)
```

Baseline commit: `57756a04d58b66a1732fd87badacf06aea31e8fb`.
Do not replace this with the failed inferred in-place rate-0/rate-1 resume path.

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
transport. No group-specific RTP/ALAC/PTP/RAOP implementation is allowed.
Mixed Native AP2 + RAOP grouping is not enabled until the Session Coordinator has
a proven shared cross-transport timeline.

## Phase 4 — retire duplicate transport integration

Only after Single, Pair, MultiRoom and hardware gates pass on MSA Core:

- remove old native Single sender;
- remove old native Pair/MultiRoom transport duplication;
- remove helper-process RAOP integration after in-process RAOP hardware parity;
- keep discovery/catalog/GUI pieces that remain useful.

## Hardware gates

Current order is intentional:

1. **locked:** Naim Native AP2 / PTP / Buffered Type-103 / 16/44.1;
2. **locked:** HomePod Native AP2 / PTP / Realtime Type-96 / 16/44.1;
3. **locked:** Apple TV5,3 Native AP2 / PTP / Realtime Type-96 / 16/44.1;
4. **locked:** AirPort10,115 Native AP2 / PTP / Buffered Type-103 / 16/44.1;
5. **open:** true RAOP-only Single receiver (`SolYan-Airplay` / N1000-SOtM) through the in-process RAOP Adapter;
6. lock the complete 16-bit Single-receiver matrix only after that RAOP gate passes hardware;
7. then resume 24-bit work;
8. then migrate Stereo Pair and MultiRoom above the proven receiver core.

CI PASS is not hardware PASS.

## Cleanup rule

Do not rename/reorganize transport internals merely to reduce file count. First
remove duplicate responsibility and prove hardware parity. File/name cleanup comes
after the new owner is real.

Do not move to 24-bit merely because the Native AP2 16-bit lanes are stable. The
true RAOP-only Single lane is part of the 16-bit core gate and must pass first.
