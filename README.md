# SAirplay2

Windows AirPlay sender rewrite by SolYan.

This repository is independent from the previous SolYan-AirPlay2 codebase and
keeps native AirPlay 2 transport behavior source-aligned with Music Assistant.

## Current transport scope

- Windows 10/11
- native AirPlay 2 and legacy/compatible RAOP
- ALAC stereo
- 16-bit / 44.1 kHz
- 24-bit / 44.1 kHz
- 16-bit / 48 kHz
- 24-bit / 48 kHz
- fixed 352 PCM frames per native packet
- Single, Stereo Pair and MultiRoom
- realtime type 96 and buffered type 103 as separate native lanes
- PTP/NTP timing, feedback and realtime retransmit
- live MultiRoom membership and bounded failed-member recovery

No 96/192 kHz target is in scope.

## Core rules

- Source lifetime is not session lifetime.
- Digital silence is PCM, not EOF.
- A Windows zero-frame loopback poll is not EOF.
- RAOP and native AirPlay 2 are separate transports.
- Realtime and buffered AirPlay 2 are separate media lanes.
- A native group shares one PTP timeline.
- 352 frames per packet is locked unless upstream or hardware evidence requires a change.
- START uses receiver-clock readiness and verified committed instants.
- GUI state must not invent protocol state.

## Development

Active development branch: `dev/hires-source-port`.

Locked stable history: `stable-1` at
`a7cb24b1faa6b54abf7d24812b732ed08eb72524`.

See:

- `docs/ARCHITECTURE.md`
- `docs/DEVELOPMENT-RULES.md`
- `docs/SOURCE-AUDIT.md`
- `docs/PROJECT-STATE.md`
