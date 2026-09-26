# SAirplay2 architecture

SAirplay2 is a Windows AirPlay sender whose native AirPlay 2 behavior is ported
from the pinned Music Assistant references rather than invented locally.

Primary references:

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`
- Music Assistant AirPlay provider (pinned project snapshot plus current stable
  cross-check)
- `philippe44/libraop@dadcfcaa26d988cdd3e3501ddf8286c224f1b494`
  for the legacy RAOP transport

Source behavior wins over assumptions. Windows substitutions are allowed below
the protocol boundary, but may not change RTSP/HAP/PTP/RTP lifecycle semantics.

## Layers

1. mDNS discovery and AirPlay/RAOP service correlation.
2. TXT + `/info` capability parsing.
3. Route and stream-lane selection.
4. HAP pairing / encrypted RTSP control.
5. NTP or PTP timing.
6. Windows WASAPI loopback capture and PCM normalization.
7. Route-specific media transport.
8. Single / Stereo Pair / MultiRoom lifecycle.
9. GUI adapter.

## Discovery and route contract

- Browse both `_airplay._tcp.local.` and `_raop._tcp.local.`.
- AirPlay TXT is the primary capability source; RAOP remains the legacy/fallback
  endpoint.
- Route selection uses capability/status/auth data, not product-name guessing.
- Stored HAP credentials use native pair-verify.
- Eligible unpaired receivers use transient HAP pairing.
- Ineligible native sessions fall back to AirPlay-compatible/RAOP where the
  advertised capabilities allow it.

Important feature meanings used by the active resolver include AirPlay 2,
pairing, PTP and buffered-audio support. Stream format capability is refined
with `/info`.

## Native AirPlay 2 connect order

The active native path preserves the source ordering:

1. TCP connect.
2. plaintext `GET /info`.
3. HAP pair-verify or transient pair-setup.
4. timing setup (PTP when available, NTP fallback when required).
5. encrypted session `SETUP`.
6. reverse event connection.
7. `RECORD`.
8. media stream `SETUP`.
9. PTP `SETPEERS` where applicable.
10. feedback / media / retransmit workers.

`RECORD` before stream `SETUP` is intentional.

## Audio formats

The native engine supports the Music Assistant target set used by this project:

- ALAC 16-bit / 44.1 kHz
- ALAC 24-bit / 44.1 kHz
- ALAC 16-bit / 48 kHz
- ALAC 24-bit / 48 kHz

The wire packetization invariant remains **352 PCM frames per packet**. 24-bit
capture uses an s32le Windows carrier, packed to s24 before ALAC encoding.

No 96/192 kHz target is in scope.

## Realtime type 96 and buffered type 103

The two media lanes are explicit and must not be conflated.

Realtime type 96:

- encrypted RTP over UDP;
- periodic sync/anchor packets;
- retransmit ring and D6 responses;
- 352 frames per packet;
- pacing/splice window, normally capped at 600 ms.

Buffered type 103:

- encrypted framed RTP over TCP;
- PTP `SETRATEANCHORTIME`;
- no realtime retransmit path;
- `FLUSHBUFFERED` for buffered warm-boundary semantics.

Automatic buffered eligibility follows the upstream policy: native AirPlay 2,
PTP, `SupportsBufferedAudio`, non-Apple, and not in the measured-hostile deny
set. Upstream currently has no deny prefixes. SAirplay2 adds one documented
hardware exception: model prefix `Mu-so Qb`, whose type-103 setup succeeded but
rendering was physically unstable; it remains native AirPlay 2 and falls back
only the media lane to realtime type 96.

## PTP and START contract

PTP receivers use the shared sender timing engine. Key invariants:

- UDP 319/320;
- Sync / Follow_Up cadence about 125 ms;
- Announce about 1 s;
- `SETPEERS` after stream setup;
- HomePod standalone follow-clock behavior remains source-aligned;
- one group shares one PTP timeline.

Cold START planning follows current Music Assistant semantics:

- solo base lead: 400 ms;
- clock-readiness wait: up to 2500 ms;
- readiness projection margin: +500 ms;
- cold group floor: 2500 ms;
- group convergence margin: 150 ms;
- group convergence: at most four rounds.

A receiver's committed START instant is authoritative. A solo receiver adopts a
forward correction without a second START. A group converges members onto the
largest verified committed instant.

## PCM, starvation and EOF

PCM amplitude is **not** stream state.

- digital-zero PCM is valid PCM;
- a temporary zero-frame WASAPI drain is starvation/no input for that poll;
- Windows loopback has no EOF sentinel, so `frames == 0` must never be promoted
  to synthetic EOF;
- starvation recovery preserves sequence, timestamp and the frozen timeline and
  restores delivery headroom with silence-pad debt;
- a true session stop/closed input is handled by the explicit lifecycle.

Amplitude/nonzero probes are intentionally not part of the active sender state.

## Retransmit and feedback

Realtime sessions retain 512 exact encrypted wire packets. Control-port
retransmit requests are answered with D6-wrapped retained RTP when available.

Feedback uses one serialized encrypted RTSP control channel:

- cadence about 2 s;
- total timeout budget 2 s;
- three consecutive misses before terminal feedback failure.

For a solo native session, hard peer/control close is terminal. In a group, a
failed member is isolated while healthy members continue and bounded recovery
may rejoin it.

## Stereo Pair and MultiRoom

Both use one Windows capture source and one shared group timeline, but they are
different user-facing session types.

Stereo Pair:

- exactly two intended members;
- shared START and PTP timeline;
- late join is used for automatic member recovery, not arbitrary user
  membership changes.

MultiRoom:

- two or more initial members;
- live add/remove is allowed;
- mixed realtime/buffered lanes may coexist when policy permits;
- common sample rate is planned at group level while per-member bit depth may be
  adapted;
- late join maps retained PCM ring data to the shared live head with prime/skip
  logic.

Failed-member recovery uses bounded backoff: 5 / 15 / 30 / 60 / 120 seconds.
Explicit Stop, removal or new playback cancels pending recovery.

## Legacy RAOP

Legacy RAOP and AirPlay-compatible legacy routes use the pinned source-built
libraop helper. SAirplay2 captures one 16-bit / 44.1 kHz PCM source and fans it
to helper processes. Runtime volume support is carried by the checked-in helper
overlay and verified during CI.

Legacy behavior is kept separate from native AirPlay 2 protocol code.

## Stability rule

Do not change the following solely to chase a symptom:

- 352 frames per packet;
- PTP cadence/identity behavior;
- feedback cadence/timeout;
- Apple automatic buffered exclusion;
- shared group PTP;
- retransmit packet geometry;
- START lead/readiness/convergence policy;
- digital-zero/EOF semantics.

Any change to those requires either a fresh hardware failure with diagnostic
evidence or a confirmed discrepancy against the pinned/current upstream source.
