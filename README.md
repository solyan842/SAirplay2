# SAirplay2

Clean Windows AirPlay sender rewrite by SolYan.

This repository is intentionally independent from the previous SolYan-AirPlay2 codebase.

## First stable target

- Windows 10/11
- ALAC
- 16-bit
- 44.1 kHz
- Stereo
- 352 frames per packet
- AirPort / RAOP route
- HomePod / AirPlay 2 native route

## Core rules

- Source lifetime is not session lifetime.
- Pause is not disconnect.
- Silence is audio data, not EOF.
- Next/seek/source changes never reset transport.
- Native AirPlay 2 RTP timeline is immutable for the session lifetime.
- RAOP and AirPlay 2 native are separate transports.
- GUI never owns protocol state.

Current phase: clean engine and clean GUI foundation.
