# HomePod Realtime Type-96 long-idle regression

Date: 2026-10-04
Branch: `dev/msa-core-architecture`
Scope: Windows adapter / Native AP2 realtime Type-96 only
Status: **INTERMITTENT — ROOT CAUSE NOT PROVEN**

## Receiver / build

- Receiver: HomePod mini `White` (`AudioAccessory5,1`)
- Route: Native AirPlay 2 / PTP / realtime type 96
- Format: ALAC 16-bit / 44.1 kHz
- Artifact under regression: Windows #1373 / artifact `11279374418`
- Naim Buffered Type-103 16/44.1 baseline is out of scope and remains locked.

## Hardware evidence

Initial connection/start is consistently clean in the supplied logs:

- `/info` advertises realtime type 96 at 44.1/16;
- route resolves to Native AP2 + PTP;
- negotiated stream is ALAC 16-bit / 44.1 kHz;
- initial `TIME requested == accepted`, delta `0ms`;
- initial diagnostics report `audio_dropped=0`, `sync_dropped=0`.

One hardware run reproduced an audible failure after a long source stop while SAirplay2 remained connected: repeated pops / choppy-distorted audio on resume.

That failing run contained a sustained run of:

`WASAPI input starvation recovery queued timeline silence.`

The supplied failing log contained roughly 525 such recovery events over about 131 seconds at the committed `STARVATION_RECOVERY_INTERVAL=250ms`.

However, a subsequent hardware retest did **not** reproduce the audible failure even though its log again showed a very long sustained sequence of the same starvation-recovery telemetry. Therefore repeated starvation recovery by itself is **not sufficient evidence of the audible failure**, and the earlier causal conclusion must not be treated as proven.

There is no transport-fatal/error signature that uniquely distinguishes the failing run yet. HomePod White / realtime Type-96 long-idle/resume is therefore an **intermittent regression under investigation**, not a proven deterministic starvation-lifecycle bug.

## Pinned-MSA comparison

Pinned `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128` has two distinct realtime behaviors that must not be conflated:

1. transient PCM starvation: `ap2cl_recover_input_gap()` pads the splice timeline so source stalls do not underrun;
2. explicit PAUSE on a hot splice timeline: `ap2cl_pause()` marks content paused while the audio loop keeps the armed wire fed with encoded silence; PLAY then hot-splices content back onto the live timeline.

Windows loopback has no explicit media-player PAUSE signal. The current adapter can remain in the starvation-recovery path during long no-frame intervals. That behavior remains a candidate area to investigate, but the second non-failing long-idle run proves that it must **not** be changed merely because the repeated starvation telemetry is present.

## Change boundary

Do not modify transport behavior yet from this evidence alone.

In particular:

- do not replace long starvation with inferred `pause_content()` / `play_content()` until a failing-vs-nonfailing trace identifies the condition that actually diverges;
- do not retune PTP, RTP, ALAC, pacing, sync, or splice depth speculatively;
- do not touch the locked Naim Type-103 path;
- do not infer pause from `AUDCLNT_BUFFERFLAGS_SILENT`; digital-zero PCM remains valid PCM.

The next safe implementation step, if more evidence is needed, is **diagnostic-only instrumentation** around starvation exit / first resumed real PCM, so a failing and non-failing resume can be compared without changing audio behavior. Useful fields include timeline head, pacing-ahead, splice pad, seq/RTP, reanchor count, PTP-anchor validity, audio/sync drop counters, and starvation duration immediately before the first resumed content packet.

## Candidate hypothesis — NOT APPROVED AS FIX

A possible Windows-only adaptation remains:

- keep short-starvation recovery for transient gaps;
- after sustained absence of all captured WASAPI frames on realtime splice, map the condition to MSA hot pause semantics;
- keep the realtime wire hot with encoded silence;
- on fresh non-silent PCM, use MSA hot play/splice semantics.

This is only a hypothesis until hardware/log evidence distinguishes it from the non-failing long-idle case. Do not implement it as a transport change solely from the existing logs.

## Acceptance gate

HomePod White / 16-bit / 44.1 kHz / realtime Type-96 is **not hardware locked** yet.

The lane must pass at least:

- initial play;
- short capture gap;
- long pause/idle of at least two minutes while SAirplay2 stays connected;
- resume after long idle with no pop, repeated thump, distortion, or silence;
- repeated long-idle/resume cycles;
- Stop/Start;
- track transitions;
- no new transport error or unexplained audio/sync drops.

Because the symptom is intermittent, one clean long-idle cycle is not sufficient to close the regression. Conversely, the presence of repeated starvation-recovery telemetry is not by itself a failure criterion.

Only after repeated hardware passes, or after a proven cause is fixed and regressed, should the HomePod Type-96 16/44.1 lane be added to the locked 16-bit matrix.
