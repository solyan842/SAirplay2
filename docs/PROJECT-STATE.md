# SAirplay2 — Project State

Last updated: 2026-09-27  
Working branch: `dev/hires-source-port`

## 1. Protected stable

Repository: `solyan842/SAirplay2`

Locked stable commit:

```
a7cb24b1faa6b54abf7d24812b732ed08eb72524
```

**Do not modify or move this stable checkpoint.**

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

## 12. Next work

Current priority is **stability preservation**, not protocol expansion.

Before changing the engine again:

1. obtain a reproducible physical failure;
2. identify which layer failed from current diagnostics;
3. compare that layer to pinned/current source;
4. make one minimal change;
5. require Windows CI PASS;
6. retest only the affected hardware path.

Do not resurrect old experiment branches as shortcuts.
