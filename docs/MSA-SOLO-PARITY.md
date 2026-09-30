# MSA SOLO parity lock

Pinned source: `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`

Branch: `dev/msa-solo-rebuild`

This document is an audit lock for the independent SAirplay2 MSA SOLO engine.
The legacy SAirplay2 engine remains frozen and is not used as the implementation base.

## Scope rule

SOLO parity means every pinned MSA behavior that can affect one receiver/session:
route selection, authentication, RTSP/HAP lifetime, timing, session commands,
RTP/TCP media, ALAC/crypto, pacing/recovery, feedback/RTX, persistent PCM
ownership, metadata/volume/progress, MediaRemote, pause/play/stop, teardown,
Windows source adaptation and RAOP-compatible fallback.

The following pinned APIs are deliberately not implemented in SOLO because
they are group/JOIN coordination contracts and belong to the later MultiRoom
coordinator:
- `ap2cl_set_start_join`
- deferred START ACK / `ap2cl_start_ack_deferred`
- JOIN clock correction that moves a member anchor
- common group START selection / late-join content-cut debt

SOLO keeps the underlying shared primitives required by those features.

## Public ap2cl API parity

| Pinned MSA API / contract | SAirplay2 MSA SOLO implementation | Status |
| --- | --- | --- |
| create/connect/destroy/disconnect | `WindowsMsaSoloClient`, `NativeSoloEngine`, RAOP owner | complete |
| connect error class/status | `NativeControlErrorClass`, `SoloConnectErrorClass` | complete |
| route AUTO/RAOP/AP2/AP2Compat | `route.rs`, `WindowsMsaSoloClient` | complete |
| force-native / PTP override / buffered / publish IP / bind IP | Windows/native config + route helpers | complete |
| HAP pair-verify | independent HAP stack | complete |
| transient pair-setup | independent HAP/SRP stack | complete |
| GET /info format capability tables | `ap2_info.rs` | complete |
| NTP timing responder | `ntp_timing.rs` | complete |
| PTP 319/320 engine + follow receiver | `ptp_engine.rs`, `native_timing_owner.rs` | complete |
| session SETUP / event / RECORD / stream SETUP / SETPEERS | concrete native control owner | complete |
| eventPort best-effort | session setup + event channel | complete |
| RECORD non-200 warning-only | `record.rs` | complete |
| realtime RTP type 96 | native media/runtime | complete |
| buffered type 103 TCP | native media/runtime | complete |
| 16-bit ALAC | source-faithful raw ALAC framing | complete |
| 24-bit ALAC | independent Apple ALAC bridge, s32 carrier -> packed s24 | complete |
| ChaCha20-Poly1305 audio | `native_codec.rs` | complete |
| first/periodic NTP/PTP sync | `native_sync.rs` | complete |
| frozen PTP anchor line | runtime/timing owner | complete |
| pacing window / 1 ms initial fill floor | native media/runtime | complete |
| UDP 20 ms deadline/drop semantics | native I/O | complete |
| buffered partial/WouldBlock pending tail | native media/I/O | complete |
| buffered nonce consumed before drain | native media | complete |
| RTX 512 ring, 0x55/0x56, exact wire replay | native RTX + worker | complete |
| stock starvation recovery | native runtime | complete |
| splice starvation/delivery recovery | native runtime | complete |
| FLUSH / FLUSHBUFFERED | native commands/transport | complete |
| START first seed / RESUME continuity | native SOLO owner | complete |
| standby / rate-0 buffered park | native transport | complete |
| pause/play/stop | native SOLO + Windows worker | complete |
| re-anchor after drained pause | native timeline/native SOLO | complete |
| audible head / warm lead / render latency / diagnostics | native SOLO diagnostics API | complete |
| content skip | SOLO fixed at zero; JOIN-only debt excluded | complete for SOLO |
| clock readiness / stall / verify observe-only | clock + native SOLO | complete |
| feedback 2 s / 2 s budget / 3 misses | feedback worker | complete |
| feedback `streams[]` diagnostics | feedback worker | complete |
| farewell TEARDOWN on timeout-dead channel | feedback/native transport | complete |
| volume | native SET_PARAMETER + RAOP equivalent | complete |
| DMAP metadata | native metadata | complete |
| artwork 15 s budget | native parameters | complete |
| progress | native parameters | complete |
| metadata placeholder before first audio | native SOLO START path | complete |
| MRP default /command | `mrp.rs` | complete |
| MRP reverse event channel / remote commands | `mrp_event.rs` | complete |
| MRP type-130 DataStream opt-in | `mrp_datastream.rs` | complete |
| type-130 dirty/15 s state push on feedback cadence | feedback pulse + DataStream worker | complete |
| MRP final STOPPED before teardown | native disconnect | complete |
| persistent input/ring ownership | persistent input / owned session | complete |
| FLUSH drain/reset barrier | Windows WASAPI worker | complete |
| 4 s / 1 MiB bounded capture ring equivalent | PCM chunker/Windows worker | complete |
| EOF/idle session semantics | owned session | complete |
| WASAPI shared loopback | independent Windows adapter | complete |
| fixed 352-frame transport chunks | PCM chunker | complete |
| RAOP-compatible fallback with pinned libraop | independent Windows RAOP owner/worker | complete |

## Exact source details locked by audit

- Native requested lead starts at 2000 ms then clamps to receiver latencyMin/Max.
- Splice default depth is 600 ms and is capped at 3000 ms.
- Buffered type 103 disables realtime splice behavior.
- Sequence and RTP offset seed follow pinned PID formulas.
- Buffered wire RTP leaves the pinned 100 ms continuation gap.
- NTP sync is 20 bytes; PTP sync is 28 bytes.
- RTX ring is 512 packets and resends byte-identical encrypted RTP.
- Feedback misses tolerate timeout-shaped failures only.
- Reverse event TCP is best-effort during connect, but once MRP owns an event
  channel its later health contributes to control health.
- MRP default path is enabled only for pair-verified sessions with a live
  reverse event channel; type-130 remains opt-in exactly like pinned MSA.
- Type-130 input is serviced continuously; state output is considered only
  after feedback cadence, with 15-second PLAYING re-push.
- First START uses fresh MSA seed; START after FLUSH uses resume continuity.
- SOLO clock verification observes readiness but never moves the committed
  anchor. Anchor correction is JOIN/MultiRoom-only.

## Hardware validation still required

Code/source parity is not a hardware PASS. Before this branch can be called a
stable SOLO build, run the new engine itself (not the frozen legacy engine)
against at least:
- HomePod mini: realtime AP2, 16-bit and advertised 24-bit formats
- AirPort Express: route-selected RAOP/AP2-compatible behavior
- pause/play, FLUSH/next/seek, standby/wake
- long feedback session
- forced packet-loss RTX observation where practical
- PTP and NTP route cases
- metadata/artwork/progress and MediaRemote where pair-verified

Hardware logs must prove connect -> timing -> START -> PCM -> feedback/RTX and
clean teardown. Any hardware discrepancy is compared back to this pinned source
before changing code.
