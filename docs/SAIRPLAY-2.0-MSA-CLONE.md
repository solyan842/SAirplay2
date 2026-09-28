# SAirplay 2.0 — MSA Clone

This branch is an independent MSA-clone implementation.

Reference sources:
- music-assistant/airplay-cli @ 431c5c582eef9307c4e39c50a0ea65e970bc1128
- music-assistant/server @ f09136859e240fc7859160e186c2e2186e917715
- libraop behavior as pinned by the project

Rules:
1. Stable3 is comparison-only and must not be modified.
2. Do not continue the previous Phase A/B/C architecture.
3. Implement MSA ownership/lifecycle first, then adapt to Windows.
4. Reuse old SAirplay code only after source-level equivalence is demonstrated.
5. No inferred playback state from PCM amplitude, digital silence or empty WASAPI polls.
6. One persistent session/source owner; transports remain transport-specific sinks.
7. START/FLUSH/STANDBY/END are explicit lifecycle operations.
8. Group orchestration follows Music Assistant server semantics, not guessed delays.
9. Hardware comparison with Stable3 happens only after this path is independently complete.
