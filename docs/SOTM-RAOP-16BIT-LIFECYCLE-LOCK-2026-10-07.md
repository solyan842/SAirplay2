# SOtM RAOP 16-bit lifecycle lock — 2026-10-07

Status: **HARDWARE-LOCKED / FIELD-PASS**

## Locked implementation commit

`c64e40dc56d7fe128aaae25846600715acbfef91`

Commit message: `fix: defer RAOP live START and re-anchor source resume`

This commit is the source-of-truth baseline for the validated SOtM/Eunhasu RAOP 16-bit lane.

## Receiver / route baseline

- Receiver: `SolYan-Airplay`
- Host: `N1000-SOtM.local.`
- Service: RAOP-only (`_raop._tcp`)
- Port: `5000`
- Route: `Raop`
- Timing: `Ntp`
- Audio: ALAC 16-bit / 44.1 kHz / 2ch
- Negotiated receiver latency: 2250 ms
- AP1 reservoir: 4224 frames (~96 ms)
- Native AP2 path: untouched by this SOtM lifecycle fix

## Field acceptance — PASS

Validated on the 1474 build/runtime log:

1. Initial Start can be armed before audio exists. Transport START is deferred until first non-silent WASAPI PCM arrives.
2. At the first PCM edge, RAOP START is committed with the preserved receiver-latency + live-source guard and the AP1 reservoir is primed before normal delivery.
3. During long source-idle gaps, the live RAOP lifecycle parks before the old audible timeline becomes stale: local PCM is flushed while the source is idle, then the session waits for fresh PCM.
4. On fresh PCM after the idle boundary, START_AFTER_FLUSH creates a new RAOP timeline and the reservoir is re-primed.
5. Observed post-start / post-resume audible head returns to approximately +2.23 to +2.25 s.
6. No runtime `ERROR` or transport-disconnected condition was observed in the accepted log.

Observed accepted events include:

- `MSA RAOP LIVE START armed ... waiting for first non-silent WASAPI PCM before transport START.`
- `MSA RAOP LIVE START committed on PCM edge ...`
- `MSA RAOP RESERVOIR primed target=4224f/96ms ...`
- `MSA RAOP LIVE SOURCE parked before stale timeline ... FLUSH complete, waiting for fresh PCM before START_AFTER_FLUSH.`

## Lock policy

For SOtM RAOP 16-bit, **do not retune or change** the following without a separate evidence-backed regression branch and explicit re-validation:

- RAOP/NTP transport selection
- ALAC 16/44.1 format baseline
- 2250 ms negotiated receiver latency handling
- 4224-frame / ~96 ms AP1 reservoir
- libraop pacing / packet cadence
- initial deferred START ownership
- source-idle park / FLUSH boundary
- START_AFTER_FLUSH resume ownership
- live-source receiver-head feasibility logic

MiTV / embedded-TV compatibility work must not modify this locked SOtM baseline. Any future compatibility logic must be isolated outside this hardware-locked path unless SOtM regression evidence proves a shared fix is required.

## Recovery rule

If later work regresses SOtM, compare against or restore the exact implementation commit:

`c64e40dc56d7fe128aaae25846600715acbfef91`

Do not use later MiTV/TV compatibility changes as the reference for SOtM behavior.
