# MiTV / compatible native-NTP receivers — accepted secondary lane

Decision date: 2026-10-28. Branch: `compat/mitv-happycast-raop-diag`.

## User decision and next-work boundary

The user explicitly accepts this lane as sufficiently stable for a secondary,
non-target receiver and requests that optimization stop here. Do not reopen
MiTV diagnostics, collect more device traffic, or retune this lane unless the
user requests it again. This is acceptance with known limitations, not proof
that every defect is fixed or a promotion into a primary hardware-locked lane.

Retain Native AirPlay 2 / NTP / realtime type 96 / ALAC 16-bit, 44.1 kHz.
Keep the compatibility session SETUP metadata and the minimum 7-ms packet gap.
The existing MSA window still controls average pacing and recovery catch-up;
do not restore the fixed-rate pacing regression from `7e11298`.

## Classification direction already implemented

Commit `9c5f28067cc236f57e076d4c6eefe99595ad45d0` selects the compatibility
profile from the measured AirPlay + RAOP capability signature (features
`0x0000001e527ffff7`, flags `0x4`). It does not match display name, Apple model,
hostname, IP, or a fixed port. Another device with this same profile requires
no per-device source edit. Other capability profiles retain existing MSA policy;
this is not a claim that arbitrary RAOP-only devices support native AP2.

Ordinary launch uses the profile; the endpoint launcher remains diagnostic.
Details and tests: [MITV-RECEIVER-CLASSIFICATION.md](MITV-RECEIVER-CLASSIFICATION.md).

At this decision, classification CI run `40720107838` was still in progress.
Do not infer CI success or new normal-launch hardware validation from this
acceptance. Audible testing in the conversation used the existing test lane.

## Known limitations deliberately retained

- Occasional audible stutter remains. Receiver logs contain intermittent
  `decrypt error,ret -86`; its exact cause is unresolved.
- Initial 100% volume can sound quieter than adjusting the slider down and
  back to 100%. Both requests are acknowledged; initialization/application
  timing remains unresolved. Moving the slider is the observed workaround.
- User-confirmed manual pauses/source changes explain the long 49/101-second
  capture-idle intervals; they are not evidence of spontaneous WASAPI failure.
- Do not assert that WASAPI, the TV speaker, network loss, or encryption alone
  is the proven root cause of all stutter based on the existing logs.

## Isolation and future direction

MiTV is a secondary compatibility lane. Preserve the separate hardware-locked
SOtM/Eunhasu RAOP, HomePod realtime, Apple TV realtime, AirPort buffered, and
Naim buffered baselines described in MSA-CORE-SOURCE-LOCK.md. Do not generalize
this receiver's workaround into those baselines or use it as their reference.

Resume work on the user's intended target devices only when requested; do not
infer a new 24-bit, Pair or MultiRoom task from this stopping decision.

Earlier evidence and historical experiments remain in MITV-NATIVE-NTP-TEST.md
and MITV-MEDIA-PACING-AUDIT.md. This decision supersedes their instructions to
keep optimizing or treat MiTV as the next blocking primary hardware gate.
