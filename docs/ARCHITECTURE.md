# SAirplay2 architecture

SAirplay2 is a clean Windows AirPlay sender rewrite.

## Layering

1. Discovery and capability parsing
2. Route resolution
3. Windows audio capture and normalization
4. Persistent PCM ring
5. Session packet clock
6. Route-specific transport
   - RAOP
   - AirPlay 2 compatible
   - AirPlay 2 native
7. GUI adapter

## Hard invariants

- Player/track lifetime is not transport lifetime.
- Pause, seek, next-track and source changes are warm boundaries.
- Starvation produces silence; it is never treated as EOF.
- Native AP2 uses one immutable RTP-to-wall-clock anchor per session.
- Sequence numbers and RTP timestamps advance through silence exactly as through music.
- RAOP and AP2-native do not share one protocol state machine.
- GUI does not own or rebuild transport sessions.
- Stop is the only user action that intentionally ends the active transport.

## Initial acceptance gates

Before real GUI playback is considered usable:

- cold start with music already playing;
- cold start while source is silent, then music starts;
- 15, 30 and 60 seconds silence then resume;
- repeated next track;
- WAV -> FLAC -> WAV;
- offline player -> browser video -> offline player;
- endpoint format/sample-rate transition;
- AirPort route;
- HomePod route;
- 30 minute continuous playback.

Any failure in these gates blocks stable release.
