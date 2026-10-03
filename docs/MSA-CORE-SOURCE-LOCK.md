# MSA Core Architecture — Source Lock

Last updated: 2026-10-03
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

## Field-proven Buffered Type-103 lifecycle baseline — LOCKED

**Scope:** this is a hardware-proven baseline for the **Buffered Type-103 Windows-adapter lifecycle only**. It is not the baseline for all 16-bit playback and is not a system-wide SAirplay2 baseline.

It specifically locks:

- how Windows capture-idle is adapted for a Buffered receiver;
- how a Buffered session crosses STANDBY/FLUSHBUFFERED and restarts;
- how fresh post-flush PCM gates deferred START;
- how deferred START uses the receiver's negotiated effective lead;
- regression expectations for this Type-103 16/44.1 lifecycle.

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
Validated: 2026-10-03

Behavior baseline commit:

`57756a04d58b66a1732fd87badacf06aea31e8fb` — `fix: honor receiver lead on buffered restart`

Validated Windows adapter lifecycle:

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

For the tested Naim, negotiated `effective_lead_ms()` is ~2000 ms. After the fix, two consecutive real Resume operations reported:

- `pacing_ahead_frames=87788` at 44.1 kHz ≈ 1.991 s;
- `pacing_ahead_frames=87583` at 44.1 kHz ≈ 1.986 s.

Both STARTs had `TIME delta=0ms`, `corrected_forward=false`, `audio_dropped=0`, `sync_dropped=0`, and the user confirmed audible Resume playback is smooth.

### Failed path — MUST NOT RETURN

Do not map Windows capture-idle directly to MSA explicit PAUSE/PLAY and then attempt an in-place `rate=0 -> rate=1` resume. Hardware evidence showed the sender could continue cleanly while the Naim remained silent.

The field-proven Windows adaptation is STANDBY/FLUSHBUFFERED + fresh PCM + deferred START with receiver-derived lead.

Do not hard-code Naim=2000 ms. Always use the receiver's negotiated effective lead with the existing minimum deferred lead floor.

## CI regression lock

The field-proven Buffered Resume path is protected by a dedicated invariant check.

Relevant commits:

- `0ae8270984a92fbe607c23fdf6e8ff0261198374` — lock field-proven buffered resume invariants;
- `b3e895f37071cbd34c126b216ee0182de1f7fcee` — enforce buffered resume baseline in CI.

The invariant must fail if the old inferred rate-1 resume path returns or if receiver-derived deferred START lead is removed.

## Source consolidation — ACTIVE NEXT STEP

The current validated artifact still depends on `scripts/apply-validated-branch-fixes.ps1` to transform parts of committed `windows_audio_worker.rs` at CI build time.

This is temporary migration debt and must now be removed without changing behavior:

1. copy the already hardware-validated Buffered idle/resume behavior into the committed Rust source;
2. copy the already validated compile fixes into committed source;
3. remove the corresponding audio/worker replacements from `apply-validated-branch-fixes.ps1`;
4. retain unrelated GUI runtime replacements until they are migrated separately;
5. require the same Buffered Resume invariant against the committed source;
6. run full Windows CI;
7. hardware-regression test the same Naim path before treating consolidation as complete.

The consolidation is a source cleanup only. It is not permission to retune timing, PTP, RTP, ALAC, pacing or lifecycle.

## Current implementation phase

We are in **MSA Core / 16-bit consolidation and regression**, not 24-bit expansion yet.

Order of work:

1. consolidate the validated runtime patch into committed source with zero behavior change;
2. repeat Naim 16/44.1 Buffered lifecycle testing: Pause/Resume, Stop/Start, track changes, short/long idle, long run;
3. regression-check HomePod 16/44.1 realtime Type-96;
4. regression-check Apple TV 16/44.1 on its selected lane;
5. keep AirPort Express explicit RAOP testing separate from native AP2;
6. lock the 16-bit matrix as a collection of lane/device baselines, not as a Naim-derived global baseline;
7. only then return to 24-bit work;
8. only after Single/Core gates are sound, continue Stereo Pair and MultiRoom migration;
9. GUI/productization remains above the transport core and must not drive protocol changes.

## 24-bit boundary

Known previous 24-bit symptoms include missing audio, repeated pops/dropouts and noisy Stop/format-transition behavior. Those symptoms are not permission to alter the now-validated Buffered Type-103 16-bit lifecycle baseline.

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
