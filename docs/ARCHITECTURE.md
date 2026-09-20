# SAirplay2 architecture

SAirplay2 is a clean Windows AirPlay sender rewrite.

The behavior reference is music-assistant/airplay-cli DESIGN.md and the current
Music Assistant AirPlay provider. Source behavior wins over assumptions in this
repository.

## Layering

1. mDNS discovery and service correlation
2. TXT capability parsing and route resolution
3. Route-specific connection preflight
4. Windows audio capture and normalization
5. One persistent PCM input/ring for the session
6. Route-specific transport
   - RAOP
   - AirPlay 2 RAOP-compatible
   - AirPlay 2 native
7. GUI adapter

## Discovery contract

- Browse both _airplay._tcp.local. and _raop._tcp.local.
- _airplay is the primary capability source.
- _raop is retained as the legacy/fallback endpoint.
- The two advertisements are correlated into one logical receiver.
- Route selection is driven by TXT feature/status bits, credentials and
  password state, not by product-name guessing.

Relevant TXT rules mirrored from the reference:

- feature 38 or 48: AirPlay 2
- feature 46 or 48: pairing-capable
- feature 41: PTP-capable
- feature 40: buffered-audio-capable
- flags 0x8: PIN required
- flags 0x200: legacy pairing
- pw=true: password advertised

## Route contract

Automatic route decision:

1. No AirPlay 2 feature -> RAOP.
2. Stored HAP credentials -> AP2 native pair-verify.
3. Pairing-capable, no PIN, no legacy flag, and password requirements satisfied
   -> AP2 native transient pairing.
4. Otherwise -> AP2 RAOP-compatible fallback.

The user-facing GUI may later expose an advanced escape hatch, but automatic
selection remains the default.

## Native AP2 connect order

The native connection sequence must preserve reference ordering:

1. TCP connect to the AirPlay RTSP port.
2. Plaintext GET /info.
3. HAP pairing:
   - stored credentials -> pair-verify (HKP:3);
   - otherwise transient pair-setup (HKP:4).
4. Timing setup:
   - PTP when selected/available;
   - NTP fallback where required.
5. Encrypted session SETUP with binary plist.
6. Open the reverse event TCP connection returned by session SETUP.
7. RECORD on the session URL.
8. Stream SETUP.

RECORD-before-stream-SETUP is intentional and must not be reordered.

## Audio target for first stable build

- ALAC
- 44.1 kHz
- 16-bit
- stereo
- 352 frames per chunk
- native realtime stream type 96 first

24-bit / 48 kHz and buffered type 103 remain out of scope until the 16/44.1
realtime path is hardware-stable.

## Persistent input contract

Track/source lifetime is not connection lifetime.

The capture/PCM producer remains attached to one session. A next-track or seek
does not rebuild pairing, crypto, timing, sockets or the session.

A warm FLUSH discards stale queued PCM so the next source cannot leak old
samples into the new track.

## Warm-boundary behavior differs by route

### RAOP / AP2 RAOP-compatible

The receiver uses the classic RAOP warm path, including RTSP FLUSH and a fresh
start mapping as required by that protocol.

### Native AP2 splice timeline

The default native realtime path keeps one frozen RTP-to-wall-clock anchor line
for the session. Warm seek/next/pause/resume/starvation recovery must not
discard the receiver buffer or reset sequence/timestamp state.

Instead:

- the input ring is flushed locally when old content must be discarded;
- the wire remains bitstream-continuous;
- missing/gap audio is encoded silence;
- RTP sequence and timestamps advance through silence exactly as through music;
- a warm START chooses the splice instant on the existing line;
- if that instant is too close to or behind the delivery head, it is corrected
  forward to head + minimum warm lead;
- the gap becomes a silence-pad debt consumed on the normal packet path.

Classic FLUSH/re-anchor remains a receiver-specific fallback path, not the
default native behavior.

## Starvation and EOF

Starvation is not EOF.

Temporary absence of PCM keeps the native realtime lane fed with encoded
silence. The session stays armed and the packet clock continues.

EOF means the whole persistent input has closed, not that one track ended.

## Hard failure contract

Encode/allocation/encryption/socket/control/session failures are terminal.
A local UDP backpressure/drop can advance the timeline as a bounded transient,
but a protocol failure must not masquerade as EOF or silently rebuild the
session behind the user's back.

## Initial acceptance gates

Before real playback is considered stable:

- cold start with music already playing;
- cold start while source is silent, then music starts;
- silence 15 s / 30 s / 60 s then resume;
- repeated next track;
- WAV -> FLAC -> WAV;
- offline player -> browser video -> offline player;
- endpoint format/sample-rate transition;
- AirPort RAOP route;
- HomePod native AP2 route;
- 30 minute continuous playback.

Any failure blocks stable release.
