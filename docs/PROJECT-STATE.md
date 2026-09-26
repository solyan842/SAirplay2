# SAirplay2 — Project State

Last updated: 2026-09-26  
Working branch: `dev/hires-source-port`

## 1. Repository / branch policy

Repository: `solyan842/SAirplay2`

Locked stable commit:

```
a7cb24b1faa6b54abf7d24812b732ed08eb72524
```

**Do not modify stable.**

Development baseline requested after Windows build #691:

```
15e0e66ed287aca7a1c9f893f75e27bc2e595b51
gui: stop legacy RAOP off the UI thread
```

GitHub Actions #691: PASS.

Current validated development head:

```
913c09e3f6eb09e8cfef0e2c143689c23674b5ee
fix: allow MSA late-join prime window
```

GitHub Actions Windows #804: PASS.

Treat the locked stable commit above as immutable. The current development head
contains the later MSA-native group work and runtime recovery validation.

Working rules:

- Compare against pinned upstream/source references before protocol/audio changes.
- Do not invent transport behavior when the referenced source defines it.
- Keep Single, Stereo Pair and MultiRoom as separate session types.
- Keep patches minimal and isolated to the failing path.
- Build with GitHub Actions before runtime testing.
- One change -> verify -> report -> continue.
- Never move the locked stable commit.

## 2. Pinned source references

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`
- `music-assistant/server@9e311eb84aba0a940bdfbf7433d5a29c07bab1b6`
- `philippe44/libraop@dadcfcaa26d988cdd3e3501ddf8286c224f1b494`

## 3. Audio ceiling

The pinned Music Assistant path used by this project supports:

- ALAC 44.1 kHz / 16-bit
- ALAC 44.1 kHz / 24-bit
- ALAC 48 kHz / 16-bit
- ALAC 48 kHz / 24-bit

No 96/192 kHz target is in scope.

## 4. Single device

Native Single is routed through `NativeSession + WindowsAudioWorker`, not the
group worker.

Confirmed architecture:

- local warm boundary cleanup only;
- no receiver RTSP FLUSH for inferred Windows track gaps;
- seq/timestamp/anchor preserved;
- silence keepalive while inferred idle;
- 352 frames per packet;
- 24-bit input uses s32le carrier -> packed s24 -> ALAC.

Single 16-bit is known-good. Single 24-bit works on HomePod; occasional tiny
clicks correlate with oversized realtime UDP packets/retransmit activity and
are not currently treated as a timeline defect.

## 5. Stereo Pair

Stereo Pair and MultiRoom are different session types.

Stereo Pair:

- exactly two members;
- shared WASAPI source;
- shared commanded START;
- member heads must remain equal;
- Pair-specific inferred-idle state;
- valid queued non-zero PCM must never be discarded at a boundary.

The queued-PCM guard fixed the previous cold-start false boundary. Runtime logs
showed `stale_nonzero_bytes=0` on valid Pair boundaries and equal member heads.

Runtime validation on HomePod mini White + Black now also confirms:

- Stereo Pair 16-bit / 44.1 kHz PASS;
- Stereo Pair 24-bit / 48 kHz PASS;
- one member may lose transport without collapsing the surviving member;
- feedback failure isolates only the failed member;
- bounded automatic rejoin runs at 5/15/30/60/120 s;
- a powered-off member can boot, reconnect on shared PTP, late-join the live
  timeline and resume automatically without Stop/Play;
- RTX remains per member and observed expired retransmits stayed at zero during
  the validated 24-bit runs.

## 6. MultiRoom

Current MultiRoom state after Pair separation:

- initial membership >=2;
- dynamic join/remove remains a MultiRoom-only user-facing feature;
- Stereo Pair may use the same late-join path only for automatic recovery;
- one shared WASAPI source and one shared PTP timeline are retained;
- common sample-rate planning is separate from per-member bit depth;
- mixed 24-bit + 16-bit groups select a common sample rate and adapt depth per
  member;
- late join uses a retained PCM ring plus prime/skip mapping aligned to the
  shared live timeline;
- realtime type 96 and buffered type 103 lanes remain distinct;
- a failed member is isolated while healthy members continue;
- bounded rejoin restores the member through the normal late-join path.

Runtime validation confirms a 2-member MultiRoom 16-bit / 44.1 kHz session
starts and fans out from one WASAPI source. Stereo Pair recovery has additionally
validated the shared late-join/rejoin machinery end-to-end.

## 7. Legacy RAOP / libraop

Legacy RAOP is a transport class, not a per-device special case.

Current behavior:

- runtime volume control is exposed for all legacy RAOP sessions;
- helper uses libraop `raopcl_set_volume()`;
- the source-built helper is verified during CI to expose
  `-V <runtime volume file>`;
- the verified helper is the exact binary copied into the artifact;
- legacy Stop cleanup runs off the egui UI thread;
- normal EOF -> drain -> disconnect is preferred;
- watchdog remains as anti-hang fallback.

The obsolete patch-based helper build file was removed after switching to the
checked-in source overlay used by CI.

## 8. Capability tables: current correction target

`/info` publishes two distinct format tables:

- `audioStream` = realtime type 96 capability
- `bufferStream` = buffered type 103 capability

These tables must not be unioned when selecting a realtime format.

Observed third-party Shairport Sync receiver example:

- realtime advertises 44.1/16
- buffered advertises 48/24
- sending realtime 24/48 produces noise instead of music
- realtime 16/44.1 plays correctly

Therefore the next capability change must make stream type explicit:

- realtime selection reads only realtime capability;
- buffered selection reads only buffered capability;
- GUI must distinguish realtime-hires from buffered-hires;
- a buffered-only 24-bit receiver must not be offered 24-bit on the realtime
  sender path.

## 9. Buffered type 103 — pinned MSA direction

Pinned `music-assistant/airplay-cli` resolves buffered audio separately from
the base AirPlay route.

Auto buffered eligibility requires:

1. native AirPlay 2 route;
2. PTP timing;
3. receiver advertises SupportsBufferedAudio (features bit 40);
4. receiver is not an Apple model;
5. receiver is not on the measured buffered-hostile deny-list.

Apple model prefixes in the pinned source include:

- AppleTV
- AudioAccessory
- iPhone
- iPad
- iPod
- Mac

A forced buffered experiment can override the Apple auto exclusion, but normal
auto routing does not.

Type 103 uses:

- encrypted RTP over TCP to receiver dataPort;
- 2-byte big-endian packet length framing;
- no RTP retransmit path;
- no realtime sync packets;
- PTP scheduling via SETRATEANCHORTIME;
- FLUSHBUFFERED + fresh anchor for warm boundaries;
- the same commanded audible instant as realtime members, allowing mixed
  buffered/realtime groups on one shared PTP timeline.

Do not implement type 103 by modifying the realtime type-96 packet loop in
place. Keep transport selection explicit.

## 10. Runtime validation status / next work

Validated on real HomePod mini hardware:

1. Stereo Pair 16-bit / 44.1 kHz: PASS.
2. MultiRoom 2-member 16-bit / 44.1 kHz: PASS.
3. Stereo Pair 24-bit / 48 kHz: PASS.
4. Per-member RTX with packets larger than 1472 bytes: PASS; no observed expired
   retransmits in the validated run.
5. Failed-member isolation: PASS; surviving member continues.
6. Automatic bounded rejoin after physical power loss: PASS.
7. Reconnected HomePod restores through shared PTP + live late-join timeline:
   PASS, without Stop/Play.

Late-join wait follows the pinned MSA 35-second prime/write allowance. This is a
maximum wait, not a fixed delay. The previous 12-second local timeout was proven
too short by a valid 24/48 join that required 2234 packets (~16.4 seconds of PCM)
to prime.

Do not change 352 frames/chunk, PTP cadence, realtime/buffered policy, START
semantics or RTX merely because 24-bit packets exceed MTU. Further engine changes
require a new runtime failure or pinned-source discrepancy.
