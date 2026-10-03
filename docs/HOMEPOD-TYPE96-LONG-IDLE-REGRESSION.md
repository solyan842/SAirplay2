# HomePod Realtime Type-96 long-idle regression

Date: 2026-10-04
Branch: `dev/msa-core-architecture`
Scope: Windows adapter / Native AP2 realtime Type-96 only

## Receiver / build

- Receiver: HomePod mini `White` (`AudioAccessory5,1`)
- Route: Native AirPlay 2 / PTP / realtime type 96
- Format: ALAC 16-bit / 44.1 kHz
- Artifact under regression: Windows #1373 / artifact `11279374418`
- Naim Buffered Type-103 16/44.1 baseline is out of scope and remains locked.

## Hardware result

Initial connection/start is clean:

- `/info` advertises realtime type 96 at 44.1/16;
- route resolves to Native AP2 + PTP;
- negotiated stream is ALAC 16-bit / 44.1 kHz;
- initial `TIME requested == accepted`, delta `0ms`;
- initial diagnostics report `audio_dropped=0`, `sync_dropped=0`.

A short source stop/idle can resume normally. A sufficiently long source stop while SAirplay2 remains connected reproduces an audible failure on resume: repeated pops / choppy-distorted audio.

The long-idle log contains a sustained run of:

`WASAPI input starvation recovery queued timeline silence.`

From the supplied log this repeats from line 163 through line 687: 525 recovery events. With the committed `STARVATION_RECOVERY_INTERVAL=250ms`, that represents roughly 131 seconds of repeated realtime starvation recovery before the observed bad resume.

There is no corresponding transport-fatal/error signature in the supplied excerpt. The failure is therefore currently scoped to the realtime timeline/content handoff after prolonged Windows capture absence, not to initial AP2 connect or the already locked Naim Buffered lane.

## Pinned-MSA comparison

Pinned `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128` has two distinct realtime behaviors that must not be conflated:

1. transient PCM starvation: `ap2cl_recover_input_gap()` pads the splice timeline so short source stalls do not underrun;
2. explicit PAUSE on a hot splice timeline: `ap2cl_pause()` marks content paused while the audio loop keeps the armed wire fed with encoded silence; PLAY then hot-splices content back onto the live timeline.

Windows loopback has no explicit media-player PAUSE signal. The current adapter leaves an arbitrarily long no-frame interval in the transient-starvation path forever, calling recovery every 250ms. Hardware evidence now shows that this is not a valid long-idle adaptation for HomePod realtime Type-96.

## Fix boundary

The next implementation must remain a Windows-adapter fix. Do not retune PTP, RTP, ALAC, pacing, sync, or the locked Naim Type-103 path.

Candidate adaptation to validate:

- keep the existing short-starvation recovery for transient gaps;
- after sustained absence of **all captured WASAPI frames** on a realtime splice session, promote the Windows condition to MSA's hot `pause_content()` semantics instead of continuing starvation recovery indefinitely;
- while inferred-paused, keep the realtime wire hot with the already implemented encoded-silence pause path;
- on fresh non-silent PCM, use MSA `play_content()` hot-splice semantics to resume content;
- never auto-resume an explicit STOP or another lifecycle command that superseded the inferred pause;
- do not infer pause from `AUDCLNT_BUFFERFLAGS_SILENT`; digital-zero PCM remains valid PCM.

This is intentionally analogous in architectural role, but not in protocol behavior, to the proven Buffered Windows idle adapter: Buffered uses STANDBY/FLUSHBUFFERED; realtime splice must use its own MSA PAUSE/PLAY hot-wire semantics.

## Acceptance gate

HomePod White / 16-bit / 44.1 kHz / realtime Type-96 is **not hardware locked** yet.

The fix must pass at least:

- initial play;
- short capture gap;
- long pause/idle of at least two minutes while SAirplay2 stays connected;
- resume after that long idle with no pop, repeated thump, distortion, or silence;
- repeated long-idle/resume cycles;
- Stop/Start;
- track transitions;
- no new transport error or unexplained audio/sync drops.

Only after that hardware gate passes should the HomePod Type-96 16/44.1 lane be added to the locked 16-bit matrix.
