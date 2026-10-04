# MSA Core Architecture — Source Lock

Last updated: 2026-10-04
Working branch: `dev/msa-core-architecture`

This file is the authoritative architecture/state lock for the current MSA Core migration branch.
If older SOLO/PROJECT-STATE documents disagree with this file, treat those conflicting sections as historical and re-check the pinned MSA sources plus current hardware evidence before changing behavior.

## Protected history

Do not modify, move, rebase onto, or force-update these protected checkpoints:

- stable1: `a7cb24b1faa6b54abf7d24812b732ed08eb72524`
- stable2: `83286f7597690297faf7f998f5beaa16a061434b`

The MSA Core migration branch was created from the SOLO rebuild line. The old native engine is not to be merged back into the MSA Core transport.

## Source of truth

Pinned references remain:

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`
- `music-assistant/server@f09136859e240fc7859160e186c2e2186e917715`
- MSA libraop submodule: `81c2182649da8645ac2a58b78e9f370c79a4165b`
- comparison/current libraop only: `dadcfcaa26d988cdd3e3501ddf8286c224f1b494`

**MSA is the source of truth for AirPlay transport behavior.**

The MSA-derived receiver core owns:

- HAP / encrypted RTSP;
- PTP / NTP;
- START / PAUSE / PLAY / FLUSH / STANDBY / STOP lifecycle;
- realtime type 96 and buffered type 103;
- RTP timeline and sequence continuity;
- ALAC framing and crypto;
- pacing / pending / recovery;
- RTX / feedback;
- metadata / MRP transport semantics.

Windows-specific code is an adapter only. It may translate WASAPI/capture lifecycle into the MSA contract when Windows has no identical source signal, but it must not invent a second AirPlay protocol contract.

Legacy AirPlay 1 / RAOP remains a separate pinned-libraop adapter and must not leak its private clock or lifecycle semantics into native AP2.

## Migration shape

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

Rules:

1. Do not merge the old native engine with MSA Core.
2. Old-engine code may be consulted only for discovery/catalog/UI requirements, group-orchestration requirements, historical hardware evidence and acceptance tests.
3. Do not independently retune PTP, RTP, ALAC, pacing, START/FLUSH or buffering when pinned MSA already defines the behavior.
4. Every transport/audio change requires pinned-source evidence, a hardware/log failure identifying the responsible layer, or a Windows-only adapter requirement needed to preserve MSA semantics.
5. CI PASS is not hardware PASS.

## Windows PCM contract

The Windows side follows the same architectural contract as the MSA server pipeline:

```text
Windows endpoint mix format
        |
private capture / conversion / resampling
        |
exact requested MSA PCM
        |
bounded PCM hub
        |
MSA receiver core
```

The producer must not block on receiver network I/O. Digital-zero PCM is valid PCM. A zero-frame WASAPI poll is not EOF.

## Field-proven Buffered Type-103 16-bit lifecycle baseline — HARDWARE LOCKED

**Scope:** this is a hardware-proven baseline for the **Naim Mu-so Qb / Native AP2 / PTP / Buffered Type-103 / ALAC 16-bit / 44.1 kHz lane**. It is not the baseline for all 16-bit playback and is not a system-wide SAirplay2 baseline.

It locks the tested Naim 16-bit lifecycle cases:

- initial play;
- capture idle / pause -> STANDBY + FLUSHBUFFERED;
- fresh-PCM deferred Resume;
- repeated Resume cycles;
- explicit Stop/Start;
- track transition.

It specifically locks:

- how Windows capture-idle is adapted for this Buffered receiver;
- how a Buffered session crosses STANDBY/FLUSHBUFFERED and restarts;
- how fresh post-flush PCM gates deferred START;
- how deferred START uses the receiver's negotiated effective lead;
- regression expectations for this Naim Type-103 16/44.1 lane.

It does **not** define or replace the independent baselines for:

- realtime Type-96 / HomePod;
- realtime Apple TV paths;
- 24-bit transport;
- RAOP / AirPlay 1;
- Stereo Pair;
- MultiRoom;
- Session Coordinator behavior as a whole.

Receiver: Naim Mu-so Qb
Mode: Native AirPlay 2 / PTP / Buffered type 103
Format: ALAC 16-bit / 44.1 kHz
Original field validation: 2026-10-03
Source-consolidated hardware regression: 2026-10-04
Full tested lifecycle confirmation: 2026-10-04

Behavior baseline commit:

`57756a04d58b66a1732fd87badacf06aea31e8fb` — `fix: honor receiver lead on buffered restart`

Validated Windows adapter idle/resume lifecycle:

```text
Streaming
   |
no captured WASAPI frames for >= 500 ms
   |
MSA STANDBY
   |
rate-0 + FLUSHBUFFERED
   |
flush every pre-boundary local PCM byte
   |
Connected / parked
   |
fresh post-flush non-SILENT PCM
   |
re-arm deferred START
   |
START at now + max(400 ms, receiver effective lead)
   |
Buffered streaming restored
```

For this Naim, negotiated `effective_lead_ms()` is ~2000 ms. The original field fix produced two consecutive smooth Resume operations at ~1.991 s and ~1.986 s pacing lead.

The source-consolidated artifact #1373 then reproduced the same behavior over four complete Resume cycles:

- `pacing_ahead_frames=87705` at 44.1 kHz ≈ 1.989 s;
- `pacing_ahead_frames=87435` at 44.1 kHz ≈ 1.983 s;
- `pacing_ahead_frames=87033` at 44.1 kHz ≈ 1.974 s;
- `pacing_ahead_frames=87777` at 44.1 kHz ≈ 1.990 s.

For all four source-consolidated Resume cycles:

- `TIME delta=0ms`;
- `corrected_forward=false`;
- `audio_dropped=0`;
- `sync_dropped=0`;
- no transport error was observed;
- the user confirmed audible playback was smooth.

The user subsequently hardware-tested **explicit Stop/Start** and **track transition** on the same Naim 16/44.1 lane and confirmed both were OK. No additional detailed transport log was supplied for those two cases, so the lock records audible hardware confirmation without inventing packet/drop metrics that were not observed in the provided log.

Therefore the tested Naim Buffered Type-103 16/44.1 Single-receiver lifecycle is now **HARDWARE LOCKED**. Do not modify this lane without new hardware/log evidence identifying a regression in this exact lane.

### Failed path — MUST NOT RETURN

Do not map Windows capture-idle directly to MSA explicit PAUSE/PLAY and then attempt an in-place `rate=0 -> rate=1` resume. Hardware evidence showed the sender could continue cleanly while the Naim remained silent.

The field-proven Windows adaptation is STANDBY/FLUSHBUFFERED + fresh PCM + deferred START with receiver-derived lead.

Do not hard-code Naim=2000 ms. Always use the receiver's negotiated effective lead with the existing minimum deferred lead floor.

## Field-proven HomePod Realtime Type-96 16-bit lifecycle baseline — HARDWARE LOCKED

**Scope:** this is a hardware-proven baseline for the **HomePod mini `White` / Native AP2 / PTP / Realtime Type-96 / ALAC 16-bit / 44.1 kHz Single-receiver lane**. It does not redefine Buffered Type-103 behavior, Apple TV behavior, 24-bit transport, Stereo Pair or MultiRoom.

Hardware-tested cases on Windows build #1397:

- initial play;
- normal continuous playback;
- repeated source-idle / resume cycles;
- long-idle resume after approximately 267 seconds;
- long-idle resume after approximately 369 seconds;
- track transitions;
- explicit Stop/Start.

The user confirmed these cases were audibly stable with no recurrence of the previous post-idle pop/chop/stutter symptom on the accepted test build.

The long-idle hardware logs show fresh capture/non-silent generations returning while the session remains `Streaming`, PTP anchor remains valid, media packets continue as `Sent`, and the observed resume boundaries report `audio_dropped=0` and `sync_dropped=0`.

The Windows realtime adapter may use MSA's existing input-gap recovery to re-establish safe realtime wire headroom when fresh non-silent PCM returns after a long capture gap. This is an adapter-level recovery only; it must not invent a separate PTP/RTP/ALAC contract or change the receiver-core semantics defined by pinned MSA.

Important diagnostic note: the temporary `starvation-exit BEFORE/AFTER first PCM` labels are broader than their wording suggests because silence/pad packets can satisfy the logging path. Treat the counters and fresh capture/non-silent generation edges as evidence; do not treat every `starvation-exit` line as proof of first real program PCM.

Therefore the tested HomePod Realtime Type-96 16/44.1 Single-receiver lifecycle is now **HARDWARE LOCKED**. Do not change this lane without new hardware/log evidence showing a regression in this exact lane.

## Field-proven Apple TV Realtime Type-96 16-bit lifecycle baseline — HARDWARE LOCKED

**Scope:** this is a hardware-proven baseline for the **Apple TV `Phòng ngủ` / model AppleTV5,3 / Native AP2 / pair-verify / PTP / Realtime Type-96 / ALAC 16-bit / 44.1 kHz Single-receiver lane**. It does not assert that every Apple TV model is identical and does not redefine HomePod, Buffered Type-103, RAOP, 24-bit, Stereo Pair or MultiRoom behavior.

The tested receiver advertises Realtime Type-96 44.1/16 and Buffered Type-103 44.1/24 + 48/24. For the 16-bit run, route selection resolved to `AirPlay2Native`, `Ptp`, pair-verified realtime, with negotiated ALAC 16-bit / 44.1 kHz.

Hardware-tested cases on the accepted 16-bit MSA Core build:

- initial play;
- normal continuous playback;
- source-idle / long-idle resume;
- repeated resume cycles;
- track transitions;
- explicit Stop/Start.

The user confirmed the Apple TV passed the full test set with no audible error. The supplied session log shows the correct 16/44.1 realtime route and clean initial transport start; no contradictory hardware symptom was reported during the tested lifecycle.

Therefore the tested Apple TV5,3 Realtime Type-96 16/44.1 Single-receiver lifecycle is now **HARDWARE LOCKED**. Do not generalize this lock to untested Apple TV models, and do not change this exact lane without new hardware/log evidence showing a regression.

## Field-proven AirPort Express Buffered Type-103 16-bit lifecycle baseline — HARDWARE LOCKED

**Scope:** this is a hardware-proven baseline for **`SOLYAN's AirPort Express` / model AirPort10,115 / Native AP2 / PTP / Buffered Type-103 / ALAC 16-bit / 44.1 kHz Single-receiver playback** on Windows build #1397. This lock follows the route actually observed on hardware; it does not classify every AirPort model as native AP2 and it does not remove the separate RAOP adapter needed for true AirPlay 1 receivers.

The accepted hardware log shows:

- negotiated ALAC 16-bit / 44.1 kHz;
- `flow: AirPlay2Native`;
- `timing: Ptp`;
- `ptp=true`;
- Buffered Type-103 deferred START after the first real WASAPI packet;
- repeated capture-idle park via STANDBY + FLUSHBUFFERED;
- fresh post-flush PCM re-arming deferred START;
- `TIME delta=0ms` on resume;
- approximately 87.9k frames of pacing lead after resume at 44.1 kHz, about 1.99 seconds;
- `audio_dropped=0` in the observed resume cycles.

The user confirmed audible playback on AirPort Express #1397 was fully stable and passed the same practical lifecycle tests used for the other Single-receiver 16-bit baselines.

Therefore the tested AirPort10,115 Native AP2 / PTP / Buffered Type-103 16/44.1 Single-receiver lifecycle is now **HARDWARE LOCKED**. Do not force this tested receiver back to RAOP based on historical assumptions; route decisions for other receivers must still follow current capability/TXT evidence and pinned MSA behavior.

## CI regression lock

The field-proven Buffered Resume path is protected by a dedicated invariant check.

Relevant commits:

- `0ae8270984a92fbe607c23fdf6e8ff0261198374` — lock field-proven buffered resume invariants;
- `b3e895f37071cbd34c126b216ee0182de1f7fcee` — enforce buffered resume baseline in CI;
- `3c12e1d840bd0c6ac4eb78e581e8abe9f88d8067` — scope the invariant explicitly to the Type-103 Windows-adapter baseline.

The invariant must fail if the old inferred rate-1 resume path returns or if receiver-derived deferred START lead is removed.

## Source consolidation — COMPLETE AND HARDWARE PROVEN FOR NAIM TYPE-103 16-BIT

Source consolidation commit:

`f0450fee062fd2b9b4e52b150043a640c451d9ef` — `refactor: promote validated buffered resume into source`

State:

1. `windows_audio_worker.rs` directly contains the validated STANDBY/FLUSHBUFFERED + fresh-PCM + deferred-START behavior;
2. the corresponding audio/worker replacements are removed from `scripts/apply-validated-branch-fixes.ps1`;
3. the runtime patch script contains GUI-only migration patches and does not own audio-worker behavior;
4. the Buffered Resume invariant passes against committed source;
5. full Windows CI #1373 passed on commit `3c12e1d840bd0c6ac4eb78e581e8abe9f88d8067`;
6. artifact `11279374418`, SHA256 `9439b7a9a8ae99643cff50c2c4cd6f56a6e5d124a69e71724679f917c41580c0`, was built from the source-aligned tree;
7. hardware regression of that artifact reproduced four smooth Buffered Resume cycles with ~1.97–1.99 s receiver-derived pacing lead and zero reported audio/sync drops;
8. explicit Stop/Start and track transition were subsequently hardware-tested and confirmed OK by the user.

Therefore the **source/runtime divergence for this Naim Buffered Type-103 16-bit lane is closed**, and its tested Single-receiver lifecycle baseline is locked.

## Current implementation phase

We are in **MSA Core / 16-bit lane regression**, not 24-bit expansion yet.

Order of work:

1. **complete and hardware locked:** Naim 16/44.1 Buffered Type-103 Single-receiver lifecycle baseline;
2. **complete and hardware locked:** HomePod 16/44.1 Realtime Type-96 Single-receiver lifecycle baseline;
3. **complete and hardware locked:** Apple TV5,3 `Phòng ngủ` 16/44.1 Realtime Type-96 Single-receiver lifecycle baseline;
4. **complete and hardware locked:** AirPort Express AirPort10,115 16/44.1 Native AP2 / PTP / Buffered Type-103 Single-receiver lifecycle baseline;
5. **next:** lock the tested 16-bit Single-receiver matrix as a collection of lane/device baselines;
6. only then return to 24-bit work;
7. only after Single/Core gates are sound, continue Stereo Pair and MultiRoom migration;
8. GUI/productization remains above the transport core and must not drive protocol changes.

## 24-bit boundary

Known previous 24-bit symptoms include missing audio, repeated pops/dropouts and noisy Stop/format-transition behavior. Those symptoms are not permission to alter the now-validated Naim Buffered Type-103, HomePod Realtime Type-96, Apple TV Realtime Type-96 or AirPort Express Buffered Type-103 16-bit baselines.

When 24-bit resumes, diagnose it as a separate format/codec/lifecycle extension on top of the appropriate locked 16-bit lane baselines.

## Drift-prevention questions

Before every future route/protocol/audio change, answer:

1. What does pinned MSA Server decide?
2. What does pinned airplay-cli do for the same lifecycle/transport operation?
3. Which layer does the hardware/log evidence identify?
4. Is the proposed code in MSA Receiver Core or only a necessary Windows adapter?
5. Which exact lane/device baseline could this change affect?
6. Does the change preserve every already field-proven baseline in scope?

If these cannot be answered, do not change transport behavior.
