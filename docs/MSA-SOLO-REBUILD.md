# MSA SOLO REBUILD — ARCHITECTURE LOCK

Source of truth: music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128.

## Non-negotiable
1. Legacy SAirplay2 Solo and MultiRoom are frozen.
2. This rebuild is an independent engine. It must not modify or depend on the legacy sairplay-engine crate.
3. SOLO is completed first. No MultiRoom, mixed-group, late-join, GUI integration or hardware-test work until SOLO parity gates are met.
4. MSA source behavior wins over prior SAirplay experiments.
5. Windows/WASAPI is an adapter to the MSA ownership/lifecycle model, not a reason to redesign that model.

## SOLO parity order
1. Persistent session owner: IDLE / PLAYING / STANDBY / ENDED.
2. Explicit START / FLUSH / STANDBY / END.
3. START returns the actual scheduled audible instant.
4. Persistent PCM ownership/ring semantics.
5. Route parity: RAOP, AP2 RAOP-compat, AP2 native.
6. Transport-specific timing: NTP/PTP and MSA feasibility rules.
7. Windows WASAPI source adapter.
8. Only after source-level parity: isolated SOLO runtime test.

No legacy engine file may be changed while completing these gates.
