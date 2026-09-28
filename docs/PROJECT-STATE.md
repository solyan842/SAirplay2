# SAirplay2 — Project State

Last updated: 2026-09-28  
Working branch: `dev/msa-cross-transport-foundation`

## 1. Protected stable

Repository: `solyan842/SAirplay2`

Locked stable checkpoints:

```
Stable1: a7cb24b1faa6b54abf7d24812b732ed08eb72524
Stable2: 83286f7597690297faf7f998f5beaa16a061434b
```

**Do not modify, move, or merge either stable checkpoint. Stable2 is the current protected stable baseline.**

The active development branch contains later native AirPlay 2, hi-res, group,
late-join and recovery work. Stable history is intentionally separate.

## 2. Source references

Pinned references:

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`
- `music-assistant/server@9e311eb84aba0a940bdfbf7433d5a29c07bab1b6`
- `philippe44/libraop@dadcfcaa26d988cdd3e3501ddf8286c224f1b494`

2026-09-27 recheck:

- `music-assistant/airplay-cli` main still points to the pinned commit.
- Music Assistant server stable was cross-checked at
  `e30a4974ba951f38e21bea8d502af3b903df992c`.
- Current MSA stable still uses the same core START constants relied on here:
  400 ms solo base, 2500 ms clock-readiness wait, +500 ms readiness margin and
  2500 ms cold-group floor.

Source-first rules and current parity status live in
`DEVELOPMENT-RULES.md` and `SOURCE-AUDIT.md`.

## 3. Last fully validated transport checkpoint

The last pre-cleanup engine checkpoint with complete Windows CI is:

```
aa38d4d866239a23e7d2da6b5539651a81b3e167
chore: make solo PTP diagnostic precise
```

GitHub Actions Windows #817: **PASS** for:

- cargo check;
- invariant tests;
- release GUI;
- ALAC24 bridge;
- source-built libraop helper;
- Windows artifact packaging.

The later cleanup series removes dead diagnostics/helpers and refreshes
documentation. It is not intended to change native wire behavior. Treat the
latest cleanup branch as validated only after its newest Windows run passes.

## 4. Native Single

Native Single uses `NativeSession + WindowsAudioWorker`.

Locked behavior:

- one persistent WASAPI source;
- 352 PCM frames per packet;
- realtime type 96 or buffered type 103 selected explicitly;
- PTP or NTP timing per source policy;
- digital-zero PCM is valid PCM;
- zero-frame WASAPI poll is not EOF;
- starvation recovery preserves the timeline;
- solo hard peer/control close is terminal;
- feedback and RTX remain independent workers.

Supported native target formats:

- ALAC 16 / 44.1
- ALAC 24 / 44.1
- ALAC 16 / 48
- ALAC 24 / 48

No 96/192 target is in scope.

### Solo START

Solo PTP cold START now follows MSA planning:

- wait for receiver clock readiness up to 2500 ms;
- base lead 400 ms;
- projection margin +500 ms;
- PCM continues to accumulate during readiness wait;
- use verified committed START;
- one receiver adopts a forward correction without a second START.

### Naim Mu-so Qb

Physical testing established two separate facts:

1. automatic buffered type 103 was unreliable on the tested Mu-so Qb even
   though setup could reach Ready;
2. realtime type 96 / ALAC 16-bit / 44.1 kHz is the correct current lane.

The model prefix `Mu-so Qb` is therefore the only local measured-hostile
buffered deny entry. It stays native AirPlay 2; only type 103 auto-selection is
denied.

The post-#817 solo hardware log shows:

- receiver clock projection reported before START;
- requested lead correctly derived from projection + 500 ms;
- committed correction explicitly recorded;
- PTP probe streak remains alive;
- delivery head settles around the expected ~600 ms realtime window;
- RTX requests are answered with zero expired packets in the observed run;
- no repeated timeline re-anchor/pad-debt pathology.

The user's current audible report is that this path is **quite stable**.
Do not modify START/PTP/RTX from this state without new failure evidence.

## 5. Stereo Pair

Stereo Pair is a distinct session type:

- exactly two intended members;
- one WASAPI source;
- one shared PTP timeline;
- shared verified START;
- per-member realtime/buffered sender where policy permits;
- per-member failure isolation and RTX;
- automatic late join is reserved for recovery, not arbitrary Pair membership.

Real-device HomePod mini White + Black validation includes:

- 16-bit / 44.1 kHz: PASS;
- 24-bit / 48 kHz: PASS;
- member failure isolation: PASS;
- surviving member continues: PASS;
- bounded automatic recovery: PASS;
- powered-off member can return through shared PTP + live late join without
  Stop/Play: PASS.

## 6. MultiRoom

Current MultiRoom behavior:

- two or more initial members;
- one shared WASAPI source;
- one shared PTP/live PCM timeline;
- live add/remove allowed;
- common sample-rate planning with per-member bit-depth adaptation;
- separate realtime96 and buffered103 lanes;
- shared cold START convergence;
- retained PCM ring for late join;
- late-join prime/skip remapped against the verified committed instant;
- failed member isolated while healthy members continue;
- bounded rejoin: 5 / 15 / 30 / 60 / 120 seconds;
- explicit Stop/removal/new playback cancels pending recovery.

The late-join outer wait is 35 seconds, matching the pinned MSA prime/write
allowance. It is a maximum wait, not a fixed delay. The old 12-second bound was
proven too short by a valid 24/48 join needing about 16.4 seconds of retained
PCM prime.

## 7. Realtime transport invariants

Do not change without new evidence:

- 352 frames per packet;
- realtime payload type 96;
- buffered payload type 103;
- 512-slot RTX history;
- D6 retransmit response;
- feedback ~2 s / 2 s timeout / 3 misses;
- PTP UDP 319/320;
- Sync/FUP ~125 ms;
- Announce ~1 s;
- shared group PTP;
- Apple/HomePod not auto-buffered;
- 600 ms realtime splice/pacing cap;
- digital-zero != EOF;
- Windows zero-read != EOF.

24-bit realtime packets may exceed ordinary Ethernet MTU. Do not shrink 352
frames merely to make those packets smaller; validated RTX already handles the
tested path.

## 8. Buffered type 103

Automatic eligibility requires:

1. native AirPlay 2;
2. PTP;
3. `SupportsBufferedAudio`;
4. non-Apple model;
5. not in the measured-hostile deny set.

Type 103 remains a separate TCP-framed media path with
`SETRATEANCHORTIME`/buffered flush semantics. It must not be implemented by
mutating the realtime packet loop.

Upstream's current measured-hostile prefix table is empty. SAirplay2's
`Mu-so Qb` entry is a documented local hardware exception.

MSA stable's automatic per-model buffer-depth default table is also empty.
Do not add a Naim-specific deeper queue without new hardware evidence.

## 9. Legacy RAOP

Legacy RAOP is a separate transport class using the source-built pinned libraop
helper.

Current behavior includes:

- 16-bit / 44.1 kHz PCM fanout;
- runtime volume control;
- source helper rebuilt and verified during Windows CI;
- normal EOF/drain/disconnect lifecycle;
- Stop cleanup outside the egui UI thread;
- watchdog only as anti-hang fallback.

Native PTP/RTP/buffered behavior must not leak into this path.

## 10. Branch audit / cleanup

A full branch audit on 2026-09-27 classified the repository as follows.

Protected/active/history roots:

- `stable-1` — immutable.
- `dev/hires-source-port` — active.
- `main` — repository/default history.
- `baseline/clean-2026-09-23` — historical baseline.
- `dev/discovery-foundation` — historical discovery baseline.

Old experiment branches whose intended behavior is already absorbed by active:

- `dev/msa-buffered-zero-pcm`
- `dev/msa-group-failure-rejoin`
- `dev/msa-group-member-recovery`
- `dev/msa-group-start-convergence`
- `dev/msa-group-zero-read-starvation`
- `dev/msa-hard-close-terminal`
- `dev/msa-mixed-group-transports`
- `dev/msa-recovery-pacing-window`
- `dev/msa-solo-start-lead-400`
- `dev/msa-solo-start-lead-400-v2`
- `dev/msa-start-convergence`
- `dev/msa-zero-pcm-not-boundary`
- `dev/msa-zero-read-starvation`
- `dev/multiroom-clock-commit`
- `dev/pair-member-diagnostics`

Explicitly superseded and **must not be merged back**:

- `dev/msa-dry-eof-keepalive`

That branch promoted prolonged Windows dry input into an EOF-style behavior.
Later source comparison proved this wrong for WASAPI: no explicit EOF exists, so
the active branch correctly keeps zero-read as starvation rather than synthetic
EOF.

Git “diverged” status is not evidence that active is missing a feature. Many
experiment ideas were later reimplemented under different commits.

## 11. Cleanup completed

The 2026-09-27 cleanup intentionally avoided wire/protocol refactoring.

Removed from active:

- first-non-silent / first-nonzero WASAPI probes;
- PCM nonzero-byte counter;
- amplitude-only boundary/resume diagnostics;
- 32-packet transition debug dumps;
- obsolete amplitude worker/session/group APIs;
- stale duplicate solo timing helpers;
- dead warm-boundary helper + self-only test;
- dead PTP summary helper;
- excessive one-second steady-state log noise.

Kept because they still discriminate real transport faults:

- PTP readiness/probe health;
- requested/committed START;
- WASAPI discontinuity;
- media-delivery anomaly;
- timeline head/pad debt at low cadence;
- RTX;
- feedback;
- member isolation/rejoin.

On `main`, the accidental empty file
`crates/sairplay-gui/assets/devices/a` was removed. Device PNG/SVG assets that
are actually referenced by the GUI were retained.

No locked stable code was modified.

## 12. Phase roadmap and source-first execution rule

Current phase status:

- **Phase A — persistent Windows PCM/session foundation: COMPLETE and hardware-tested.**
  Persistent WASAPI producer + bounded ring, source starvation != EOF,
  2000 ms render lead separated from the 250 ms START feasibility floor,
  and capture independence from START/PTP/ALAC/network blocking are locked.
- **Phase B — cross-transport foundation: IN PROGRESS.**
  Common START convergence, FLUSH/head contracts, shared PCM source/fan-out
  contracts, RAOP PCM sink, native PCM sink, producer/reader ownership split,
  native + legacy source-injection seams, one-read shared PCM pump,
  session-level shared PCM coordinator, failure pruning and explicit member
  removal now exist. Mixed AP2 + RAOP runtime is still deliberately disabled.
- **Phase C — heterogeneous handoff: NOT STARTED.**
  Per-member 44.1/48 kHz conversion and wider mixed-format handoff must not be
  pulled into Phase B.
- **Phase D — orchestration completion: PENDING.**
  Full late-join matrix, sync_adjust and related common orchestration.
- **Phase E — third-party compatibility and controls: PENDING.**
  Route override/fallback, featureless AP2 handling, buffer-depth and interface
  controls only after the common engine is stable.

### Permanent design rule

Music Assistant / airplay-cli remains the primary architectural reference for
session ownership, PCM fan-out, START/FLUSH/timeline semantics, member failure
isolation and mixed transport orchestration.

Do **not** copy implementation mechanically when SAirplay2 has a different
problem domain. SAirplay2 keeps its proven Windows-specific strengths where
appropriate, especially persistent WASAPI capture/ring semantics and desktop
runtime/GUI integration.

The target architecture is therefore:

**MSA orchestration semantics + SAirplay2 Windows capture/runtime strengths.**

Any intentional deviation from MSA must answer all three questions before code:

1. What does pinned/current MSA do?
2. Why must SAirplay2 differ for this Windows/system-audio use case?
3. What source, invariant test or hardware evidence proves the deviation is
   safer/better?

Without that evidence, follow MSA.

### Critical mixed-transport invariant

Mixed AP2 + RAOP must not be enabled until runtime has one explicit path:

```
ONE Windows PCM source read
        -> SAME source chunk
        -> fan out to all active native + RAOP sinks
        -> wait for every member write / isolate failures
        -> advance to the NEXT source chunk
```

Never implement mixed transport by cloning a `WindowsPcmSourceHandle` and
letting native and RAOP independently call `read_shared_pcm()`. The ring is a
single-consumer byte timeline; independent readers would split/steal bytes
instead of receiving the same source chunk.

### Current Phase B head

Latest validated engine head:

`1d68355b1197effa539f5f432d594c686c04647d`
— explicit shared PCM member removal — Windows Action #905 PASS.

Immediately preceding validated milestones:

- `3a10379951b9d3a1491f8efbf2ae3dd83bcdd680`
  — coordinator cycle test alignment — Windows Action #904 PASS.
- `e026b2cfaceb7dec5d9826ae9d4ae1f4296e3ef2`
  — scoped fan-out compile fix; superseded by #904 PASS.
- `9bbc1071eb10f7266e8c3536aa635a32b4b02a5d`
  — prune failed PCM members between shared reads; initial Actions failed only
  on compile/test wiring and were corrected without changing runtime semantics.
- `3525473d402ccedda91dd2170f68940aabf094db`
  — session-level shared PCM coordinator — Windows Action #901 PASS.
- `cc463af5683830574887e948367ddc9a49a30325`
  — one-read shared PCM pump contract — Windows Action #900 PASS.
- `36682a24bbe082beffa84747cba63d5684ee71ae`
  — native PCM sends routed through the common sink contract — Windows Action
  #899 PASS.
- `df02ee9bea251ee49d77f74823ce27a7eff3b5d8`
  — native shared PCM source injection seam — Windows Action #897 PASS.
- `fd268cf33803f9159b8901bcf57cdc61a9f2621e`
  — legacy shared PCM source injection seam — Windows Action #896 PASS.

Current shared-PCM invariants are now explicit and tested:

1. one coordinator owns exactly one source reader;
2. each pump cycle reads the source once;
3. the exact same source chunk is delivered to every active sink;
4. temporary starvation performs no fan-out and is not EOF;
5. all member write results are gathered before failure isolation;
6. failed sinks are removed before the next source read;
7. explicit higher-level member removal is available before the next read;
8. duplicate participant identities are rejected;
9. native and RAOP both have sink/source seams, but they are **not yet wired**
   into one live mixed session.

### Next architectural step

Do **not** remove the mixed-group GUI guard yet.

The next safe Phase B task is to inspect the current native and legacy session
lifecycles and introduce the smallest higher-level mixed-session ownership seam
that can instantiate:

```
one WindowsPcmSession owner
        -> one WindowsPcmSourceHandle consumed only by the coordinator
        -> NativePcmSink + LegacyPcmSink participants
        -> coordinator pump cycles
```

The key constraint remains that the existing native/legacy workers must not
continue independently reading cloned source handles once the coordinator owns
the live feed. START, PTP/NTP, splice/pad, late-join history, RAOP helper
lifecycle and teardown must remain transport-specific until each lifecycle
handoff is source-checked against MSA.

Before wiring live mixed audio, require a code path where **the coordinator is
provably the only PCM reader**. Only then may mixed runtime be enabled for a
same-rate/same-compatible-format Phase B test. 44.1/48 per-member conversion
remains Phase C and must not be hidden in this step.

### Explicitly forbidden shortcuts

- no stable2 modification or merge;
- no GUI mixed-group guard removal before the common coordinator exists;
- no sharing one ring by independent native/RAOP readers;
- no Naim-specific delay/buffer workaround without new hardware evidence;
- no forced buffered type103 for Apple/HomePod;
- no PTP/RTX rewrite;
- no arbitrary buffer increase;
- no synthetic EOF/silence semantics for continuous WASAPI;
- no live FLUSH exposure without a source-defined content boundary;
- no AppleTV 401 protocol patch without evidence that receiver availability is
  not the cause;
- no Phase C resampling hidden inside Phase B.

### Long-project memory rule

This file is the repository source of truth for cross-chat continuity. After
every important architectural milestone, update this document with:

- current branch/head;
- last PASS/FAIL Action;
- phase status;
- source evidence;
- hardware evidence;
- newly locked invariants;
- explicitly rejected approaches;
- exact next safe step.

Conversation memory is secondary to the repository state.

## 13. Next work

Current priority is **complete Phase B without breaking the validated native/legacy paths**. Stability preservation remains mandatory; protocol expansion must follow the phase gates above.

For the cross-transport work now underway:

1. inspect the exact native + legacy lifecycle call sites before every patch;
2. preserve one-reader ownership and same-chunk fan-out;
3. make one minimal source-aligned change;
4. require Windows CI PASS before the next change;
5. do not enable mixed GUI/runtime until coordinator ownership is real;
6. when live mixed runtime is finally introduced, hardware-test same-rate,
   same-compatible-format first before any Phase C conversion work.

For unrelated transport regressions, still require reproducible hardware
evidence before modifying START/PTP/RTP/RTX/buffer behavior.

Do not resurrect old experiment branches as shortcuts.
