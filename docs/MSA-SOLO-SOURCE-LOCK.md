# MSA SOLO Source Lock

Last updated: 2026-10-01  
Working branch: `dev/msa-solo-rebuild`

This file is a deliberate architecture lock. If implementation and this file
disagree, re-check the pinned MSA sources before changing behavior.

## Source of truth

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`
- `music-assistant/server@f09136859e240fc7859160e186c2e2186e917715`
- MSA libraop submodule:
  `81c2182649da8645ac2a58b78e9f370c79a4165b`

Comparison-only legacy/current libraop pin:

- `dadcfcaa26d988cdd3e3501ddf8286c224f1b494`

## Non-negotiable rule

**MSA is the Source of Truth for the independent SOLO AirPlay 2 path.**

Before modifying any audio/protocol behavior, first establish exactly how the
pinned MSA implementation does the same job.

Do not invent a different mechanism merely because it appears simpler on
Windows.

## Locked behavior

- Route selection follows pinned MSA policy.
- START, FLUSH, pacing, recovery and timing semantics follow pinned MSA.
- Realtime packetization remains 352 frames unless pinned source/evidence says
  otherwise.
- ALAC framing follows pinned source.
- Volume follows pinned MSA/libraop RTSP `SET_PARAMETER` semantics.
- Time domains remain separated as documented in
  `docs/MSA-SOLO-TIME-DOMAIN.md`.
- Windows-specific code is an adapter only.

## Windows PCM contract

Pinned MSA server feeds cliairplay an exact per-player PCM stream after
conversion/resampling.

The Windows path must mirror that architecture:

```
Windows endpoint mix format
        |
        v
private capture/conversion/resampling
        |
        v
exact MSA transport PCM
        |
        v
native AP2 / pinned transport behavior
```

Therefore:

- capture the shared-mode endpoint in its real `GetMixFormat()`;
- do not force the Windows endpoint to 44.1/16 or 48/24 transport PCM;
- convert privately to the requested MSA PCM;
- 16-bit gate target: 44.1 kHz / s16le / stereo;
- 24-bit gate target: 48 kHz / 24-bit carried as s32le / stereo before ALAC;
- reset converter history at the same content/flush boundary where MSA replaces
  its per-player conversion pipeline.

## Evidence discipline

A change is allowed only when at least one of these exists:

1. direct pinned-source evidence;
2. a hardware/log failure that identifies the responsible layer;
3. a Windows-only adaptation required to satisfy the exact MSA contract.

Do not change PTP, RTP, ALAC, recovery, buffering or endpoint behavior merely
because another symptom appears nearby.

CI PASS is not hardware PASS.

## Protected history

Never modify or move:

- stable1 `a7cb24b1faa6b54abf7d24812b732ed08eb72524`
- stable2 `83286f7597690297faf7f998f5beaa16a061434b`

## Hardware gates

### Gate 1 — Black SOLO 16/44.1

**PASS — 2026-10-01**

Observed:

- Windows local audio remains normal;
- Black audio has correct speed/pitch;
- previous pop/drop/missing-audio symptoms are not present in the tested tracks;
- live receiver volume works and returns RTSP 200;
- immediate START timing is sane.

### Gate 2 — Black SOLO 24/48

**NEXT.**

Required diagnostic should show the actual Windows mix format and the target
MSA transport PCM, for example:

`MSA INPUT ... -> MSA PCM 48000 Hz / s32le / 2ch`

Do not proceed to Stereo Pair or MultiRoom until this gate is evaluated.

## Drift prevention

If a future patch would make SAirplay2 behave differently from pinned MSA,
the patch must explicitly document:

- what MSA does;
- why Windows cannot use that behavior directly;
- the smallest adapter needed;
- evidence that the adapter preserves MSA semantics.

Otherwise, do not merge the deviation.
