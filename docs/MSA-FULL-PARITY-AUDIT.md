# SAirplay2 — Full Music Assistant parity audit

Status: **audit only — no runtime changes in this branch**

Baseline under audit:

- SAirplay2 stable-2: `83286f7597690297faf7f998f5beaa16a061434b`
- music-assistant/airplay-cli: `431c5c582eef9307c4e39c50a0ea65e970bc1128`
  - this is still the current upstream main HEAD at audit time
- music-assistant/server current dev: `f09136859e240fc7859160e186c2e2186e917715`
- libraop reference: `dadcfcaa26d988cdd3e3501ddf8286c224f1b494`

The purpose of this document is to stop incremental one-off patching. It compares
the full streaming contract across solo, stereo-pair, multi-room, mixed AirPlay
families and third-party receivers. A runtime change should not be made until the
affected rows in this matrix have been closed as one coherent source-aligned
change.

## Status vocabulary

- **MATCH** — the active SAirplay2 stable-2 behavior implements the relevant MSA
  contract closely enough that no source parity gap was found.
- **GAP-P0** — architectural or timing divergence that can affect correctness
  across a broad path. Close before further tuning.
- **GAP-P1** — important compatibility/synchronization surface implemented by
  current MSA but absent or materially different in SAirplay2.
- **GAP-P2** — robustness/operability difference which is not a known blocker
  for the currently validated hardware.
- **N/A** — Music Assistant feature that belongs to its queue/media-server
  product layer and is not required for a Windows system-audio transmitter.
- **LOCAL DIVERGENCE** — deliberate, evidence-backed SAirplay2 hardware rule;
  do not remove just to make a textual upstream match.

---

# 1. Executive result

Stable-2 has already ported a large part of the AirPlay 2 wire engine correctly:
HAP, encrypted RTSP, PTP/NTP timing primitives, SETPEERS, realtime RTP,
retransmit, type-103 buffered transport, SETRATEANCHORTIME,
FLUSHBUFFERED, 352-frame packetization, 16/24-bit ALAC, receiver format probes,
clock-readiness policy, late-join constants and native group recovery.

The remaining differences are **not** best handled as isolated device patches.
The important gaps form four coherent layers:

1. **Input/session layer** — SAirplay2 does not yet use the MSA persistent reader
   + session ring contract for live Windows PCM.
2. **Timeline/lead separation** — stable-2 uses the 250 ms warm feasibility
   quantity as its native render lead, while MSA keeps a distinct 2000 ms
   anchor-to-render lead and a separate 250 ms minimum START floor.
3. **Cross-transport / heterogeneous group layer** — current MSA can operate
   sessions containing RAOP and AirPlay 2 members and can hand different PCM
   formats to individual members; SAirplay2 currently blocks those cases.
4. **Third-party escape/tuning layer** — current MSA exposes explicit timing,
   compatibility-route, buffer-depth and sync-adjust controls that SAirplay2
   does not yet expose.

These are the items that should be closed as batches before any further
model-specific tuning.

---

# 2. Core audio input/session engine

## 2.1 Persistent input reader and session ring — GAP-P0

### MSA contract

`airplay-cli/src/ap2_session.c` explicitly defines the threading model:

- one persistent reader thread drains the PCM input fd;
- it writes to one ring for the whole stream session;
- the audio loop only calls `ap2_session_read()/poll()`;
- the input descriptor is non-blocking;
- reader poll interval is 30 ms;
- ring depth is 4 seconds of media with a 1 MiB minimum;
- `ready_bytes` gates the first `[STATUS] audio buffered_ms=...` signal;
- FLUSH parks the reader, resets the ring, drains old bytes to EAGAIN, re-arms
  the audio-ready signal, and resumes the same persistent reader.

Current Music Assistant server confirms the producer side: per-member ffmpeg
writes PCM to the persistent cliairplay stdin. Replacing ffmpeg does not close
that stdin.

### SAirplay2 stable-2

The native Windows workers drain WASAPI and drive transport sending in the same
active worker loop. There is no active equivalent of MSA's `ap2_session`
reader/ring between the live Windows producer and the AirPlay sender.

`pcm_ring.rs` is not that equivalent: it is a simple older i16 ring used by
the generic session scaffolding and has different starvation semantics
(`pop_or_silence`).

### Why this matters

The #848 Naim diagnostic captured:

- ordinary RTP gaps ~11–12 ms and ~600 ms delivery head;
- then sender gaps of 41/29/25/27/218 ms;
- WASAPI discontinuities during the same stall cluster;
- further 68/161/136 ms gaps;
- delivery head collapsing from ~600 ms to single digits;
- no corresponding RTX request burst causing the event.

That observation is consistent with the active producer and sender sharing one
execution path. MSA's source contract specifically decouples input draining from
the transport audio loop.

### Closure rule

Port the **semantics of `ap2_session.c`**, not an invented queue:

- persistent producer reader;
- bounded session ring;
- source-aligned readiness;
- source-aligned FLUSH/reset lifecycle;
- blocking/wakeup behavior;
- no synthetic EOF from temporary source starvation.

The Windows adaptation may replace a Unix input fd with WASAPI as the producer,
but the buffering/lifecycle contract should stay source-equivalent.

This must cover both solo and native group Windows audio workers, rather than
being a Naim-only fix.

---

# 3. Native AirPlay 2 timeline and render lead

## 3.1 Anchor-to-render lead vs START feasibility floor — GAP-P0

This audit found a significant quantity conflation.

### MSA has two different concepts

`airplay-cli/src/cliairplay.c`:

- playback `lead_ms` default is **2000 ms**;
- it is described as anchor-to-render lead;
- no ordinary server-side knob changes that base lead.

`airplay-cli/src/ap2_client.c`:

- the 2000 ms lead is clamped to the receiver-reported
  `latencyMin/latencyMax` window;
- PTP/NTP sync and frozen anchor math use this effective lead;
- the separate `AP2_MIN_WARM_LEAD_MS` is **250 ms** and is only the minimum
  commanded START feasibility floor;
- splice pacing depth is separately **600 ms**;
- pacing margin is separately **250 ms**;
- assumed receiver buffering is **2000 ms**, therefore the default delivery
  ceiling is 1750 ms before the splice-depth cap is applied.

Current server `stream.py` repeats this explicitly: the binary owns a
2000 ms playback lead, while receiver queue depth is a separate `--latency`
concept.

### SAirplay2 stable-2

`NativeSessionConfig::new()` defaults `lead_frames = 11_025`, which is
250 ms at 44.1 kHz. That value is then clamped to receiver latency min/max and
used by realtime PTP/NTP sync mapping.

The pacing calculation itself is mostly source-aligned:

- 250 ms margin;
- 1750 ms assumed receiver window after margin;
- 600 ms splice depth.

Therefore the mismatch is not "600 ms versus 2000 ms". It is that the
**render lead** and **minimum START floor** are currently represented by the
same 250 ms-sized value in stable-2.

### Closure rule

Separate the source concepts exactly before changing any device-specific START
lead:

- render lead: MSA default 2000 ms, receiver-clamped;
- warm START feasibility floor: 250 ms;
- caller START planning lead: server-side 400/500/2500 policy;
- splice pacing depth: 600 ms default;
- delivery margin: 250 ms.

Do not hard-code a Naim-specific 2–3 second START as a substitute for this
separation.

---

# 4. Realtime type-96 transport

## 4.1 352-frame packetization — MATCH

- fixed 352 PCM frames per transport packet;
- no packet shrinking to fit MTU.

## 4.2 ALAC 16/24-bit path — MATCH

- 16-bit s16le input;
- 24-bit Windows s32le carrier;
- 24-bit truncation/packing before ALAC;
- 44.1/48 kHz native target set.

## 4.3 Realtime RTP encryption and timing — MATCH

The stable implementation carries the required encrypted RTP and timing packets
and keeps NTP and PTP paths separate.

## 4.4 Pacing calculation — MATCH, except render-lead issue above

Stable-2 uses the source-equivalent 250 ms margin, 1750 ms assumed receiver
window and 600 ms splice cap.

## 4.5 Retransmit ring / request handling — MATCH

- 512 packet history;
- receiver retransmit requests are answered from already encrypted wire packets;
- ring expiration semantics match source intent;
- #848 measurement showed useful requests arriving at only 18–39 ms packet age
  and replies in tens/hundreds of microseconds.

Do not enlarge or rewrite RTX to fix the observed Naim worker stall.

---

# 5. Buffered type-103 transport

## 5.1 Eligibility — MATCH with one intentional local rule

MSA auto-buffered route requires:

- native AirPlay 2;
- PTP;
- SupportsBufferedAudio;
- non-Apple receiver;
- not deny-listed.

SAirplay2 follows that shape for solo.

### LOCAL DIVERGENCE: Mu-so Qb

Current MSA buffered deny-list is empty. SAirplay2 locally denies automatic
type-103 routing for model prefix `Mu-so Qb` because physical tests showed:

- SETUP/anchor accepted;
- actual type-103 rendering was silent/intermittent/stuttering;
- realtime type-96 ALAC 16/44.1 works.

Keep this as an evidence-backed local deny entry unless later hardware evidence
reverses it.

## 5.2 Buffered TCP backpressure — MATCH

The pending-tail behavior treats timeout/would-block as backpressure rather than
as immediate session death.

## 5.3 SETRATEANCHORTIME retries — MATCH

Stable-2 implements the current source constants:

- up to 12 attempts;
- 500 ms spacing;
- immutable commanded START instant;
- recomputed remaining lead per retry.

## 5.4 FLUSHBUFFERED boundary behavior — MATCH

The current implementation preserves the sender sequence/timestamp cut point and
clears the anchor for re-anchor.

---

# 6. PTP / NTP

## 6.1 PTP feature selection — MATCH for automatic route

SupportsPTP selects PTP and non-PTP native devices use NTP.

## 6.2 Shared PTP for native groups — MATCH

Native multi-member sessions share one PTP engine/timeline rather than creating
one independent grandmaster per member.

## 6.3 HomePod OS 27 receiver-clock follow rule — MATCH

SAirplay2 has the source rule for standalone HomePod shape:

- AudioAccessory model;
- `igl=1`;
- no `pgid`;
- no `tsid`;
- OS 27+.

Stereo-pair / Apple-TV-output shapes are excluded as in upstream.

## 6.4 PTP clock readiness/stall — MATCH at primitive-policy level

Current source values are present:

- full third-party lock window ~2300 ms;
- Apple fast seat after three exchanges / 250 ms settle;
- 5000 ms high-confidence stall threshold;
- server planning wait 2500 ms and additional 500 ms readiness lead.

## 6.5 Explicit PTP/NTP escape hatch — GAP-P1

Current MSA offers per-device advanced streaming modes:

- Automatic;
- AirPlay 2 PTP;
- AirPlay 2 NTP for non-Apple receivers;
- AirPlay 2 compatibility mode;
- AirPlay 1 RAOP when available.

This is specifically an escape for third-party devices that advertise PTP but
do not behave correctly.

SAirplay2 stable-2 does not expose an equivalent route/timing override surface.

Do not add NTP as an option for Apple receivers: current MSA deliberately hides
that lane because measured Apple hardware renders silence there.

---

# 7. Solo session

## 7.1 Connection / HAP / encrypted RTSP — MATCH

The native chain is present:

- plaintext /info;
- HAP transient or pair-verify;
- encrypted RTSP;
- session/media SETUP;
- timing setup;
- RECORD;
- feedback/event channels.

## 7.2 PIN pairing / stored credentials — MATCH

The GUI and engine support interactive PIN setup and stored HAP credentials for
subsequent pair-verify.

## 7.3 Solo clock readiness planning — broadly MATCH

The current worker uses the receiver projection and source-derived lock policy.

However, the final START/render behavior must be re-evaluated after closing
Sections 2 and 3, because current source buffering and render lead are not yet
parity-complete.

## 7.4 Solo unexpected receiver teardown — policy difference, P2

Current SAirplay2 intentionally suppresses hidden automatic reconnect for a hard
solo peer/control failure. MSA's player/server layer can cold-restart a stream
under its own queue/player lifecycle, but a solo process death is not treated as
a group rejoin.

For a desktop system-audio transmitter, suppressing an invisible reconnect is a
product-policy choice rather than a wire-parity bug. Keep it explicit and do
not conflate it with group-member rejoin.

---

# 8. Stereo Pair

MSA does not require a separate transport protocol named "StereoPair"; the
important invariant is that pair members participate in one synchronized member
set on the same commanded timeline.

## 8.1 Pair discovery from `tsid` — MATCH for the tested HomePod shape

SAirplay2 groups two HomePods with the same `tsid` and does not collapse a
single device into a fake pair.

## 8.2 Shared PTP / shared START — MATCH in core native path

The pair follows the same native group timing machinery.

## 8.3 16/44.1 and 24/48 physical validation — MATCH on validated hardware

Existing physical tests passed for the HomePod mini stereo pair.

## 8.4 Pair input/session ring — GAP-P0 inherited from Section 2

The native group worker still needs the MSA-style persistent input/session
layer. Fix this once for native group, not separately for Pair.

## 8.5 Per-member format handoff — GAP-P0/P1 inherited from Section 10

A homogeneous HomePod pair does not expose this, but the group engine should not
depend on all member sample rates matching.

---

# 9. Native MultiRoom

## 9.1 Cold shared start — mostly MATCH

Current policy constants match current server:

- cold group floor 2500 ms;
- readiness timeout 2500 ms;
- readiness +500 ms;
- convergence correction margin 150 ms.

## 9.2 START dispatch concurrency — GAP-P1

Current MSA sends member START operations concurrently, collects their
acknowledged committed instants, and reconverges on the maximum corrected
instant.

SAirplay2's native Windows group convergence currently arms members
sequentially inside the convergence loop.

This is a real orchestration difference. It is not by itself proof of an
audible fault on the already-validated HomePod pair, but it increases skew risk
when one member has a slower control path or buffered-anchor retries.

Close it when the group orchestration layer is normalized; do not patch an
individual device around it.

## 9.3 Member failure isolation — MATCH

The active group can continue after one member fails.

## 9.4 Automatic bounded rejoin — MATCH in policy

The 5/15/30/60/120 second recovery ladder is present for group members.

## 9.5 Late join buffer/history — MATCH in principal policy

The group implementation carries:

- 2500 ms late-join headroom floor;
- 12 s history floor;
- 2 s history margin;
- 6 MiB hard cap;
- clock-readiness wait;
- mapping of retained PCM to a committed join instant.

## 9.6 Late-join test coverage is narrower than current MSA — GAP-P1 test debt

Current MSA explicitly tests:

- empty ring;
- ring predating requested position;
- prime from ring tail;
- group reanchor shift;
- clock-ready projection;
- no-projection fallback;
- stalled join clock rejection;
- feed continuing while clock readiness and START ack are outstanding;
- cancellation during ack;
- content mapping from acknowledged instant;
- silence padding when retained history is short;
- frame alignment of a short ring;
- bounded residual shortfall.

SAirplay2 has several late-join tests, but not the complete current upstream
behavioral matrix. Before calling MultiRoom complete, mirror these contracts in
the Rust test suite.

---

# 10. Heterogeneous member formats

## 10.1 Per-member bit-depth adaptation — MATCH

The native group worker can downconvert a 24-bit source packet for a 16-bit
member.

## 10.2 Per-member sample-rate adaptation — GAP-P0

SAirplay2 explicitly reports:

`multi-room per-member sample-rate conversion is not available yet`

when member sample rates differ.

Current MSA does not require every member to use the same handoff format.
Each member gets its own ffmpeg output/handoff format selected from the shared
session format and that player's capabilities.

Therefore a group containing members that resolve to different 44.1/48 kHz
handoffs is an upstream-supported case but not a stable-2 case.

Closure must be at the **per-member handoff layer**, not a rule that forces every
receiver to one arbitrary format unless current MSA itself makes that choice.

---

# 11. Mixed AirPlay 2 + legacy RAOP MultiRoom

## 11.1 Mixed transport group — GAP-P0

SAirplay2 stable-2 explicitly blocks a selection containing native AirPlay 2 and
RAOP members and asks the user to select one transport family.

Current MSA's stream-session tests explicitly cover combinations of:

- AirPlay 2 only;
- RAOP only;
- multiple AirPlay 2;
- multiple RAOP;
- mixed RAOP + AirPlay 2.

MSA's timing comments also state that a shared commanded audible instant is the
cross-protocol contract.

## 11.2 Legacy path does not use current MSA unified session lifecycle — GAP-P0

Current MSA RAOP path uses the same persistent `ap2_session` source engine:

- audio arrives through the persistent session ring;
- START has an explicit commanded audible instant;
- infeasible START is corrected forward and acknowledged;
- FLUSH preserves the connection and re-anchors;
- standby/pause/resume have explicit state transitions;
- next delivery head is projected from libraop playtime.

SAirplay2's legacy path uses the older `cliraop` helper and a custom
`LegacyGroupSession`:

- custom 5000 ms legacy group start lead;
- custom per-member writer queues;
- different source/session lifecycle.

This makes mixed native+RAOP grouping difficult to implement correctly on top
of the current stable paths.

### Closure rule

Do not bolt a "mixed group bridge" onto the two existing unrelated workers.
First normalize the session/timeline contract so both transport families expose
the same required operations and acknowledged audible-time semantics, following
current MSA.

---

# 12. Third-party receiver routing

## 12.1 Normal feature-based route resolution — MATCH in common cases

Stable-2 resolves:

- legacy RAOP;
- AirPlay 2 native transient;
- AirPlay 2 pair-verify;
- RAOP-compatible AirPlay 2 fallback;
- PTP/NTP from feature support.

## 12.2 Featureless AirPlay2-only receiver — GAP-P1

Current MSA server has an explicit case:

when discovery gives an AirPlay service but **no RAOP fallback**, it forces
AirPlay 2 even if the TXT feature bits are empty/unparseable. The binary's
explicit AIRPLAY2 route also treats `features == 0` as pairable.

SAirplay2 records `txt_was_empty`, but stable route resolution does not use it
to produce the equivalent AP2-only fallback. A sparse third-party receiver can
therefore be sent toward legacy RAOP incorrectly.

## 12.3 Explicit compatibility route — GAP-P1

MSA exposes an AirPlay 2 RAOP-compatible escape lane. SAirplay2 does not expose
the equivalent per-device selection.

## 12.4 Explicit RAOP escape — GAP-P1

Where both services exist, MSA lets the user pin RAOP. Stable-2 does not expose
the same advanced route choice.

---

# 13. Third-party receiver queue/depth tuning

## 13.1 Automatic stock depth — MATCH

The normal realtime splice depth is 600 ms.

## 13.2 Per-device buffer-depth override — GAP-P1

Current MSA retains an advanced receiver queue-depth override. Current options
include values through 3000 ms. The family default table is currently empty
because auto-buffered type-103 removed the need for former blanket LinkPlay
defaults, but the manual escape hatch remains.

SAirplay2 hard-codes the stock depth and does not expose the corresponding
per-device override.

This should be implemented as the source's coherent splice-depth override,
where all consumers read the same effective depth, not as an arbitrary extra
buffer inserted in the Windows worker.

---

# 14. Per-device synchronization adjustment

## 14.1 sync_adjust — GAP-P1

Current MSA exposes AirPlay sync adjustment and includes that offset in group
anchor/convergence calculations.

SAirplay2 stable-2 has no equivalent per-device adjustment.

This is useful for heterogeneous physical receivers whose actual acoustic
render point differs even when network/timeline math is correct.

Do not use it to hide an engine timing bug; it is a calibrated user/device
offset after the common timeline is correct.

---

# 15. Discovery and networking

## 15.1 mDNS AirPlay/RAOP discovery — MATCH for current tested fleet

The current fleet is discovered with both service families and the device model
metadata used by route decisions.

## 15.2 Explicit interface / publish-IP controls — GAP-P2

Current MSA has tested behavior for:

- leaving interface choice automatic;
- pinning a local interface;
- separately publishing a reachable address when necessary;
- avoiding redundant publish address when it equals the bound interface.

This exists for multi-homed/container/network edge cases.

SAirplay2 normally derives the local path from the socket/preflight and does not
expose an equivalent advanced interface/publish-IP control surface.

Do not prioritize this ahead of P0/P1 streaming parity unless a real
multi-homed failure appears.

## 15.3 IPv6 PTP — not an upstream parity target

The current airplay-cli PTP implementation itself remains IPv4-oriented.
Do not treat lack of IPv6 PTP in SAirplay2 as a missing MSA feature.

---

# 16. Volume

## 16.1 Percent-to-dB curve — MATCH

SAirplay2 follows the current 0.3 dB/percentage-point mapping and mute floor
behavior.

## 16.2 Receiver-owned volume feedback / echo suppression — partial GAP-P2

Current MSA has application-layer handling for:

- receiver volume ownership;
- mute ownership;
- suppressing a short echo window after its own volume command;
- adopting external device changes afterward.

SAirplay2 can send receiver volume and the GUI retains its own volume setting,
but it does not mirror all of Music Assistant's player-ownership abstraction.

For a single desktop controller this is not a wire/audio correctness blocker.
Only port what is needed for real two-controller feedback behavior.

---

# 17. Pause / standby / warm replace / queue transitions

Current MSA has a rich persistent media-session lifecycle:

- pause/resume without destroying the connection;
- standby/park;
- warm track replacement;
- FLUSH all members before STARTing any;
- source EOF withholding while a replacement track is pending;
- content-cut correction and media-position reconciliation.

SAirplay2 is a Windows **system-audio loopback transmitter**, not a
queue/track-aware media server.

Therefore:

- the transport-level primitives that affect continuous system audio remain
  relevant;
- queue-transition and track-replacement orchestration is **N/A** unless
  SAirplay2 later becomes a track-aware player.

Do not port Music Assistant queue semantics merely for textual parity.

The important reusable part is the common session/input layer and the
cross-transport START/FLUSH contract described above.

---

# 18. Metadata / MRP / remote controls / announcements

Current MSA has substantial additional product features:

- now-playing metadata and artwork;
- MRP remote-control data channel;
- announcements/ducking;
- remote button events.

These are not required to solve system-audio transport stability or
Solo/Pair/Multi synchronization.

Classification: **N/A for current parity milestone**, unless SAirplay2 product
requirements explicitly add them later.

Do not allow these features to delay the core parity closure.

---

# 19. Error and recovery behavior

## 19.1 Group member isolation/rejoin — MATCH in principal policy

Already implemented and physically validated for native groups.

## 19.2 Clock stall detection — MATCH

Keep the high-confidence 5000 ms stall verdict separate from the shorter
2500 ms planning wait.

## 19.3 Receiver reset after long host/system stall — open validation item

The #848 log also captured a separate ~12.8 s sender/clock outage followed by
receiver RTSP reset. That event is much larger than ordinary pacing jitter and
should not be "fixed" by enlarging a receiver buffer.

After the MSA input/session layer is ported, repeat the long-run test. If a
12-second whole-process/system/network scheduling outage remains, diagnose it as
a separate host/network suspension event.

---

# 20. Full parity matrix

| Area | Solo | Pair | Native Multi | Mixed AP2/RAOP | 3rd-party | Status |
|---|---|---|---|---|---|---|
| HAP / encrypted RTSP | Yes | Yes | Yes | AP2 side | Yes | MATCH |
| PIN pair + stored verify | Yes | Yes | Yes | AP2 side | Yes | MATCH |
| 352-frame ALAC | Yes | Yes | Yes | Both families conceptually | Yes | MATCH |
| 16/24-bit AP2 | Yes | Yes | Yes | AP2 side | Yes | MATCH |
| 44.1/48 AP2 | Yes | Yes | homogeneous | blocked if per-member rate differs | partial | GAP-P0 group handoff |
| Type-96 RTP | Yes | Yes | Yes | AP2 side | Yes | MATCH |
| RTX | Yes | Yes | Yes | AP2 side | Yes | MATCH |
| Type-103 | Yes | capable | code-capable | AP2 side | eligible devices | MATCH core |
| PTP/NTP primitives | Yes | Yes | Yes | transport-specific | Yes | MATCH |
| Shared PTP | N/A | Yes | Yes | AP2 members | Yes | MATCH |
| HomePod OS27 follow | Yes | pair excluded correctly | yes per member shape | AP2 side | N/A | MATCH |
| Persistent MSA input ring | No | No | No | No unified layer | No | **GAP-P0** |
| 2000 ms render lead separated from 250 ms START floor | No | No | No | No | No | **GAP-P0** |
| Concurrent group START fanout | N/A | not full parity | not full parity | absent | relevant | GAP-P1 |
| Late join | N/A | group machinery | Yes | not mixed | Yes | MATCH core / test debt |
| Member failure isolation | N/A | Yes | Yes | not mixed | Yes | MATCH |
| Rejoin ladder | N/A | Yes | Yes | not mixed | Yes | MATCH |
| Mixed AP2 + RAOP group | N/A | possible conceptually | No | **No** | important | **GAP-P0** |
| Per-member sample-rate handoff | N/A | homogeneous only | No | No | important | **GAP-P0** |
| Featureless AP2-only force | incomplete | same | same | same | important | GAP-P1 |
| Route/timing override | No | No | No | No | important | GAP-P1 |
| Buffer-depth override | No | No | No | No | important | GAP-P1 |
| sync_adjust | N/A | No | No | No | important | GAP-P1 |
| Interface/publish-IP override | No | No | No | No | edge cases | GAP-P2 |
| Queue track-replace semantics | N/A | N/A | N/A | N/A | N/A | N/A |
| Metadata/MRP/announcements | optional | optional | optional | optional | optional | N/A milestone |

---

# 21. Closure order — one coherent programme, not one-off patches

## Phase A — source/session foundation (P0)

Close as one coherent branch:

1. Port MSA `ap2_session` input buffering semantics to the Windows live source:
   persistent producer, 4 s / 1 MiB minimum ring, source-aligned readiness and
   FLUSH lifecycle.
2. Separate MSA's 2000 ms receiver render lead from its 250 ms START
   feasibility floor and from the 600 ms splice depth.
3. Apply those common semantics to both native solo and native groups.
4. Re-run Naim/HomePod/AppleTV baseline before any model-specific tuning.

No third-party depth tweak should land before this phase is validated.

## Phase B — one common cross-transport group contract (P0)

1. Normalize native AP2 and legacy RAOP around:
   - commanded audible START;
   - acknowledged/corrected actual START;
   - warm FLUSH/re-anchor;
   - current delivery head;
   - source readiness.
2. Replace the older custom legacy group startup contract with the current MSA
   RAOP session semantics where applicable.
3. Permit one group session to contain RAOP and AP2 members.
4. Verify shared audible instant with mixed hardware.

Do not bolt a mixer onto two incompatible worker lifecycles.

## Phase C — heterogeneous member handoff (P0/P1)

1. Follow current MSA's per-member handoff-format concept.
2. Permit different 44.1/48 kHz member outputs from one session source.
3. Keep 24->16 depth conversion source-aligned.
4. Test mixed bit depth + mixed sample rate + mixed transport.

## Phase D — group orchestration parity (P1)

1. Concurrent START dispatch.
2. Maximum committed-instant convergence.
3. Complete late-join test contract.
4. Per-device `sync_adjust`.

## Phase E — third-party compatibility surface (P1/P2)

1. Featureless AP2-only route.
2. Advanced route modes: auto/PTP/NTP/compat/RAOP with MSA eligibility rules.
3. Per-device splice depth override with the same 3000 ms ceiling.
4. Optional interface/publish-IP controls for multi-homed systems.

Only after A–E should new receiver-specific exceptions be considered.

---

# 22. Validation matrix after closure

No stable promotion until all relevant cells pass on real hardware or a
source-faithful transport harness.

## Solo

- HomePod / HomePod mini;
- Apple TV;
- Naim Mu-so Qb realtime96;
- non-Apple AP2 PTP receiver;
- non-Apple AP2 NTP receiver if available;
- RAOP-only receiver;
- featureless/sparse AP2-only receiver if available.

For each applicable device:

- first start;
- repeated start;
- 30+ minute run;
- volume changes;
- temporary source silence;
- Windows scheduling load;
- network loss;
- receiver reboot/reset;
- 16/44.1;
- 24/44.1;
- 16/48;
- 24/48 where supported.

## Stereo Pair

- cold start;
- 16/44.1;
- 24/48;
- member loss;
- survivor behavior;
- pair recovery.

## Native MultiRoom

- two members;
- three members;
- realtime96 only;
- mixed realtime96 + buffered103 where eligible;
- late join;
- remove member;
- member crash;
- 5/15/30/60/120 rejoin;
- heterogeneous bit depth;
- heterogeneous sample rate.

## Mixed transport

- AP2 + RAOP;
- AP2 pair + RAOP member;
- AP2 24-bit member + RAOP 16-bit member;
- late join of each transport family;
- one member loss and rejoin;
- shared audible start verification.

## Third-party stress

- advertised PTP but forced NTP;
- forced AP2 compatibility;
- forced RAOP;
- explicit splice depth;
- sync adjustment;
- sparse/featureless AP2 discovery.

---

# 23. Rules after this audit

1. Do not change PTP, RTX, START, buffer depth or model routing from one log line
   without first mapping the change to the source contract in this document.
2. Do not add a receiver-specific constant when a missing MSA layer explains the
   behavior more generally.
3. A local hardware deny-list entry is allowed only with a repeatable physical
   test showing that the advertised path is accepted but does not render
   correctly.
4. Stable-2 stays immutable.
5. One parity phase should be implemented and validated as a complete unit
   before moving to the next; do not interleave unrelated tuning patches.
6. Update this matrix whenever upstream Music Assistant changes a contract used
   by SAirplay2.

