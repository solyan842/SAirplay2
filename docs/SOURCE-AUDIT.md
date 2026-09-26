# SAirplay2 source parity audit

This document describes the current active architecture. It replaces the old
early-alpha audit snapshot.

## References

Primary native reference:

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`

The upstream `airplay-cli` main branch was rechecked during the 2026-09-27
cleanup and still points at the same commit.

Server/lifecycle references:

- project-pinned `music-assistant/server@9e311eb84aba0a940bdfbf7433d5a29c07bab1b6`
- current stable cross-check:
  `e30a4974ba951f38e21bea8d502af3b903df992c`

Legacy RAOP:

- `philippe44/libraop@dadcfcaa26d988cdd3e3501ddf8286c224f1b494`

The server layer is consulted for session orchestration such as receiver-clock
readiness, START convergence, late join and warm lifecycle. The C client remains
the primary reference for native wire behavior.

## Evidence labels

- **SOURCE** — directly matched to the reference implementation.
- **CI** — compiled/tested by the Windows workflow.
- **HW** — observed on physical hardware.
- **LOCAL-HW-EXCEPTION** — intentionally differs from an empty/default upstream
  table because a real receiver demonstrated a reproducible problem.
- **NOT-APPLICABLE** — Music Assistant application behavior that does not map to
  a Windows system-audio sender.

## Current parity matrix

| Area | Active SAirplay2 state | Audit result |
|---|---|---|
| AirPlay + RAOP discovery | Both services browsed and correlated | SOURCE / CI |
| TXT feature parsing | AirPlay2, pairing, PTP, buffered, auth/password state preserved | SOURCE / CI |
| `GET /info` | Realtime and buffered format tables parsed separately | SOURCE / CI |
| HAP transient pairing | Implemented | SOURCE / CI / HW |
| Stored HAP pair-verify | Implemented when credentials are available | SOURCE / CI |
| Encrypted RTSP | Shared serialized control/CSeq path | SOURCE / CI / HW |
| Native ordering | info -> HAP -> timing -> session SETUP -> event -> RECORD -> stream SETUP -> SETPEERS | SOURCE / CI / HW |
| PTP / NTP choice | PTP where supported, NTP fallback when required | SOURCE / CI / HW |
| PTP ports/cadence | 319/320, Sync/FUP ~125 ms, Announce ~1 s | SOURCE / CI / HW |
| HomePod follow-clock | Source predicate/path retained for standalone Apple receiver cases | SOURCE / CI / HW |
| Realtime type 96 | UDP encrypted RTP + sync + RTX | SOURCE / CI / HW |
| Buffered type 103 | TCP-framed encrypted RTP + SETRATEANCHORTIME + buffered flush | SOURCE / CI / HW on tested routes |
| Apple buffered auto policy | Apple models excluded from automatic type 103 | SOURCE / CI |
| Mu-so Qb buffered policy | Type 103 denied by local model prefix after physical instability; native AP2 retained on realtime96 | LOCAL-HW-EXCEPTION |
| ALAC formats | 16/44.1, 24/44.1, 16/48, 24/48 native target set | SOURCE / CI; tested subsets HW |
| Packet geometry | 352 PCM frames per native packet | SOURCE / CI / HW |
| 24-bit carrier | Windows s32le -> packed s24 -> ALAC | SOURCE-aligned adaptation / CI / HW |
| Realtime pacing | receiver window with 250 ms margin, capped by 600 ms splice depth | SOURCE / CI / HW |
| Digital-zero PCM | ordinary PCM; never a boundary | SOURCE / CI / HW |
| Zero read/starvation | temporary no-input recovery; never synthetic EOF | SOURCE / CI / HW |
| RTX | 512 retained exact wire packets, D6 response | SOURCE / CI / HW |
| Feedback | ~2 s cadence, 2 s budget, 3 consecutive misses | SOURCE / CI / HW |
| Solo hard peer close | terminal; no hidden auto-reconnect | SOURCE / CI |
| Group member failure | failed member isolated, survivors continue | SOURCE / CI / HW |
| Bounded rejoin | 5/15/30/60/120 s, cancelled by user lifecycle changes | SOURCE-aligned server behavior / CI / HW |
| Solo START | wait clock readiness, base 400 ms, +500 ms readiness margin, adopt committed correction | SOURCE / CI / HW |
| Group cold START | 2500 ms floor, clock projection, verified commits, max four convergence rounds | SOURCE / CI / HW |
| Late join | shared timeline, ring prime/skip, verified commit, bounded 35 s prime wait | SOURCE / CI / HW |
| Native metadata | native metadata control exists | SOURCE / CI |
| Legacy RAOP | pinned source-built libraop helper, runtime volume overlay | SOURCE pin / CI / HW |

## START parity

Current Music Assistant stable still uses:

- receiver-clock readiness timeout: **2500 ms**;
- readiness margin: **500 ms**;
- solo base START lead: **400 ms**;
- warm group lead: **500 ms**;
- cold group lead: **2500 ms**.

SAirplay2 follows the same planning model.

For a single PTP receiver, audio bytes are allowed to accumulate while clock
readiness is resolved. The requested anchor is no earlier than the base lead or
the projected readiness plus 500 ms. The committed receiver instant is
authoritative; a single receiver adopts a forward correction without issuing a
second START.

For a group, all members share one audible instant. If any member commits later,
the group reconverges on the largest verified instant with the source-aligned
fan-out margin, up to four rounds.

The Mu-so Qb physical comparison was a useful proof of this distinction:
standalone playback was unstable on the old fixed-400-ms path, while a group
with HomePod naturally received the longer clock-readiness planning. The later
solo path now reports projection before START and maintains a stable delivery
head on the tested run.

## PCM / starvation parity

Music Assistant's `audio_present` means the first input **bytes** have arrived.
It does not inspect PCM amplitude.

Therefore active SAirplay2 intentionally:

- does not use “first non-silent” or “first nonzero” state to start or splice;
- treats Windows silent buffers as valid zero PCM;
- treats an empty WASAPI drain as a temporary no-input poll;
- never promotes prolonged `frames == 0` to EOF;
- preserves the immutable realtime timeline while starvation recovery adds
  silence headroom.

The earlier experiment that introduced “EOF-style silence keepalive” from a dry
Windows input is superseded and must not be reintroduced.

## Buffered parity and local exception

Upstream's measured-hostile buffered deny-list is currently empty.

SAirplay2 keeps the same deny-list mechanism but has one local hardware entry:

- model prefix: `Mu-so Qb`

Reason: the physical receiver accepted native type-103 setup/anchor but produced
silence/intermittent rendering. Realtime type 96 on the same native session is
stable enough to continue testing. This exception does not force RAOP and does
not apply to other Naim models without evidence.

Music Assistant stable currently also keeps its automatic per-model buffer-depth
override table empty. SAirplay2 therefore does not invent a deeper Naim queue.

## Hardware validation snapshot

The active development history contains real-device evidence for:

- HomePod mini Stereo Pair 16-bit / 44.1 kHz;
- HomePod mini Stereo Pair 24-bit / 48 kHz;
- two-member MultiRoom 16-bit / 44.1 kHz;
- member isolation while the surviving HomePod continues;
- bounded power-loss recovery and automatic live late join;
- per-member RTX with zero expired retransmits in the validated runs;
- Mu-so Qb native realtime 16-bit / 44.1 kHz after the solo START correction:
  clock projection is observed before START, committed correction is reported,
  PTP remains alive and delivery head stays around the expected ~600 ms window.

A hardware observation applies only to the exact tested route/format. It is not
permission to generalize a device-family workaround.

## Application-layer differences that are not protocol defects

Music Assistant owns a media queue and explicit track/seek/replacement
transitions. SAirplay2 owns Windows system audio.

The following MSA application features therefore do not map 1:1 and are not
treated as missing native-wire parity:

- queue-level predicted replacement EOF;
- provider/media loading state;
- Music Assistant announcement mixing;
- Sendspin/controller integration;
- queue-owned seek/next metadata lifecycle.

Where SAirplay2 has an equivalent transport event, it must still preserve the
same wire/timing invariants.

## Branch audit — 2026-09-27

The repository contained 21 branch refs at the start of cleanup.

### Keep as active or protected history

- `stable-1` — immutable locked stable.
- `dev/hires-source-port` — active development.
- `main` — repository history/default branch; one accidental empty file was
  removed during cleanup.
- `baseline/clean-2026-09-23` — historical clean baseline.
- `dev/discovery-foundation` — historical discovery baseline (same old tip as
  the clean baseline at audit time).

### Historical experiments whose correct behavior is already absorbed

- `dev/msa-buffered-zero-pcm`
- `dev/msa-group-failure-rejoin`
- `dev/msa-group-member-recovery`
- `dev/msa-group-start-convergence`
- `dev/msa-group-zero-read-starvation`
- `dev/msa-hard-close-terminal`
- `dev/msa-mixed-group-transports`
- `dev/msa-recovery-pacing-window`
- `dev/msa-solo-start-lead-400`
- `dev/msa-solo-start-lead-400-v2`
- `dev/msa-start-convergence`
- `dev/msa-zero-pcm-not-boundary`
- `dev/msa-zero-read-starvation`
- `dev/multiroom-clock-commit`
- `dev/pair-member-diagnostics`

Do not merge these wholesale. Their intended behavior is already represented in
the active branch, often by later source-aligned implementations with different
commit ancestry.

### Explicitly superseded experiment

- `dev/msa-dry-eof-keepalive`

This branch converted prolonged dry Windows input into an EOF-style keepalive.
Later MSA comparison established that Windows `frames == 0` has no EOF
semantics; the active branch correctly uses starvation recovery and forbids
synthetic EOF. This old branch must not be resurrected.

Git ancestry alone is not a parity signal: several historical experiment
branches show as “diverged” because the same behavior was reimplemented or
ported later under different commits.

## Cleanup completed during this audit

Active development cleanup removed only state with no remaining production
meaning:

- first-non-silent / first-nonzero WASAPI amplitude probes;
- nonzero-byte counting in the PCM chunker;
- amplitude-only transition diagnostics and large packet dumps;
- obsolete non-silent worker/session/group APIs;
- duplicate stale solo timing helpers left behind after clock-readiness logic
  moved to the canonical worker/sender path;
- a dead warm-boundary helper and self-only test;
- a dead PTP summary helper;
- excessive steady-state diagnostic frequency (reduced to low cadence).

On `main`, the accidental empty file
`crates/sairplay-gui/assets/devices/a` created by the historical “Create a”
commit was removed.

No locked stable code was modified.

## Do-not-regress list

Do not change without new source discrepancy or hardware evidence:

- 352 frames per packet;
- PTP timing cadence;
- realtime/buffered lane separation;
- Apple auto-buffered exclusion;
- digital-zero / EOF semantics;
- solo/group START constants and verified-commit behavior;
- shared group PTP;
- RTX geometry/history;
- feedback timeout policy;
- late-join shared timeline;
- bounded member recovery.

The goal after this audit is stability, not further protocol churn.
