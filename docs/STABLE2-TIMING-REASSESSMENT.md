# Stable2 Timing Reassessment Baseline

Branch: `dev/stable2-timing-reassessment`
Base: `83286f7597690297faf7f998f5beaa16a061434b`

## Baseline rule

Stable2 is the source of truth for native Windows playback behavior.
Do not replace its native-only WASAPI -> chunker -> pacing -> sender path with
persistent/shared PCM ownership intended for future mixed transport.

## Stable2 timing behavior already present

Stable2 already includes the timing behaviors worth preserving:

- Cold START waits for one complete 352-frame PCM packet.
- PTP receiver-clock readiness is observed before START.
- Requested START lead is the max of normal cold-start lead and receiver-clock readiness lead.
- START is resolved against a receiver-clock/floor constraint before commit.
- Native sender pacing uses `can_accept_frames()` before consuming queued PCM.
- Digital-zero PCM is treated as valid audio, not as stream-state.
- Input-gap recovery is triggered only after a real dry-input interval of at least 250 ms.
- Gap recovery adds splice silence without redefining ordinary PCM cadence.
- PTP probe health and steady timeline diagnostics are observational only.
- TEARDOWN occurs while the audio path is still hot; timing is retired afterwards.
- Native Stereo Pair / MultiRoom use the existing shared timing and pacing path proven on hardware.

## Evidence from the regressed Phase A/B branch

Do NOT port the following behavior back into native-only playback:

- `WindowsPcmSession` persistent ring replacing Stable2's direct WASAPI/chunker loop.
- 250 ms blocking `read_exact_timeout()` as the primary realtime native read cadence.
- Starvation timer starting only after that blocking timeout.
- Persistent source backlog accumulating during long PTP readiness waits.
- Mixed/common coordinator owning normal native-only PCM.
- Mixed worker pacing that bypasses the proven native `can_accept_frames()` gate.

These changes were useful experiments for mixed-transport architecture, but they
changed native runtime behavior and produced regressions versus Stable2.

## Phase B material that remains reusable later

Keep as reference on `dev/msa-cross-transport-foundation`, but do not merge into
native-only runtime yet:

- transport-neutral common START convergence
- cross-transport timeline helpers
- RAOP external-feed control/START seam
- shared PCM fan-out/coordinator contract
- native external sink seam
- mixed session ownership shell

Any future mixed implementation must sit beside Stable2 native playback rather
than underneath/replacing it.

## Next evaluation sequence

1. Build this exact Stable2-based branch without audio changes.
2. Hardware-test the same known receivers and capture baseline logs.
3. Compare startup lead, head delta, PTP probe behavior, RTX, gap recovery and stop behavior.
4. Only then introduce one timing-only change at a time, each with an A/B result.
5. Mixed transport work resumes only after native-only remains indistinguishable from Stable2.

## Hard guard

If a proposed change modifies `windows_audio_worker.rs` or
`windows_multiroom_worker.rs` native playback semantics, it requires direct
hardware/log evidence first. Architecture preference alone is not sufficient.
