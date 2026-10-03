# HomePod Realtime Type-96 long-idle regression

Date: 2026-10-04
Branch: `dev/msa-core-architecture`
Scope: Windows adapter / Native AP2 realtime Type-96 only
Status: **INTERMITTENT — ROOT CAUSE NOT PROVEN**

## Receiver / build

- Receiver: HomePod mini `White` (`AudioAccessory5,1`)
- Route: Native AirPlay 2 / PTP / realtime type 96
- Format: ALAC 16-bit / 44.1 kHz
- Primary source-consolidated baseline build: Windows #1373 / artifact `11279374418`
- Additional failing trace supplied from Windows #1376.
- Diagnostic build #1379 is intended to capture the exact starvation-exit boundary without changing transport behavior.
- Naim Buffered Type-103 16/44.1 baseline is out of scope and remains locked.

## Hardware evidence

Initial connection/start is consistently clean in the supplied logs:

- `/info` advertises realtime type 96 at 44.1/16;
- route resolves to Native AP2 + PTP;
- negotiated stream is ALAC 16-bit / 44.1 kHz;
- initial `TIME requested == accepted`, delta `0ms`;
- initial diagnostics report `audio_dropped=0`, `sync_dropped=0`.

The failure is intermittent. One long-idle run resumed normally even while printing a very long sequence of:

`WASAPI input starvation recovery queued timeline silence.`

Therefore repeated starvation-recovery telemetry alone is **not** a failure criterion and is not sufficient proof of cause.

### Failing #1376 field trace

A later hardware run from Windows #1376 reproduced a stronger failure: after a long no-frame interval White lost audible output; selecting/changing several tracks eventually restored sound.

The second session in that log again starts clean (`TIME delta=0`, no initial audio/sync drops), then remains in realtime starvation recovery for a long period. During the bad period the receiver starts requesting retransmissions:

- first diagnostic: `requested=2`, `answered=0`, `expired=2`, `audio_dropped=2`, `pacing_ahead=16043f`, `reanchors=337`, `splice_pad=349f`;
- next diagnostic: `requested=4`, `answered=0`, `expired=4`, `audio_dropped=2`, `pacing_ahead=21422f`, `reanchors=338`, `splice_pad=1763f`.

At 44.1 kHz those pacing-ahead values are only about 0.36–0.49 s, while the same session began at `pacing_ahead=122792f` (about 2.78 s). This comparison is diagnostic correlation only; initial-start and long-starvation states are not assumed equivalent.

The all-expired retransmit requests are a more useful discriminator than the starvation log spam: the receiver was asking for realtime packets that were no longer answerable from the sender's retransmit history at that moment. That can explain why the control session can remain nominally alive while audible output is lost, and why later track transitions can eventually recover the receiver by moving the content/timeline boundary. It is **not yet proof** that retransmit expiry itself is the root cause.

There is still no transport-fatal/error signature that uniquely proves which state first diverged. HomePod White / realtime Type-96 long-idle/resume therefore remains an **intermittent regression under investigation**.

## Pinned-MSA comparison

Pinned `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128` has two distinct realtime behaviors that must not be conflated:

1. transient PCM starvation: `ap2cl_recover_input_gap()` pads the splice timeline so source stalls do not underrun;
2. explicit PAUSE on a hot splice timeline: `ap2cl_pause()` marks content paused while the audio loop keeps the armed wire fed with encoded silence; PLAY then hot-splices content back onto the live timeline.

Windows loopback has no explicit media-player PAUSE signal. The current adapter can remain in the starvation-recovery path during long no-frame intervals. That behavior remains a candidate area to investigate, but the non-failing long-idle run proves that it must **not** be changed merely because the repeated starvation telemetry is present.

The pinned realtime retransmit history is finite (512 packets, roughly 4.1 s at 44.1/352-frame packets). The Rust core mirrors that size. The #1376 trace therefore raises a concrete question for the diagnostic build: at starvation exit, is the receiver requesting sequence numbers from a pre-gap timeline region that has become unavailable, or has another timeline/PTP/splice state already diverged before the request arrives?

## Change boundary

Do not modify transport behavior yet from this evidence alone.

In particular:

- do not replace long starvation with inferred `pause_content()` / `play_content()` until a failing-vs-nonfailing trace identifies the condition that actually diverges;
- do not enlarge the RTX ring or change resend semantics speculatively;
- do not retune PTP, RTP, ALAC, pacing, sync, or splice depth speculatively;
- do not touch the locked Naim Type-103 path;
- do not infer pause from `AUDCLNT_BUFFERFLAGS_SILENT`; digital-zero PCM remains valid PCM.

The next safe implementation step is the already prepared **diagnostic-only #1379 instrumentation** around starvation begin/exit and the first resumed real PCM. A useful failing-vs-good comparison must include timeline head, pacing-ahead, splice pad, seq/RTP, reanchor count/shift, PTP-anchor validity, audio/sync drops, starvation duration, capture/non-silent generations and flush generations immediately before and after the first resumed content packet.

## Candidate hypotheses — NOT APPROVED AS FIXES

Two candidate mechanisms now deserve comparison, not implementation:

1. a prolonged Windows no-frame interval should eventually map to MSA hot pause/play semantics rather than remain transient starvation forever;
2. the receiver's realtime sequence/playout expectation diverges during the long gap, leading to retransmit requests for unavailable pre-gap packets and an inaudible live session until a later track boundary repairs it.

Either, both, or neither may be the actual cause. #1379 exists to distinguish them without changing audio behavior.

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
