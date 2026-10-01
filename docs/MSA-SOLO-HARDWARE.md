# Independent MSA SOLO hardware harness

Uses only sairplay-msa-solo; frozen GUI/legacy engines are not invoked.
Extract the entire MSA-SOLO-Test-Windows artifact. Run from PowerShell:

```powershell
.\msa-solo-test.exe White.local 7000 ap2 44100 16 2>&1 | Tee-Object solo-homepod.log
```

For interactive commands, run directly in a console; type quit for explicit teardown.
Audio comes from the Windows default playback device through real WASAPI loopback.
Initial volume is 50%. Counters are transport evidence, not proof of audible playback.

Arguments: HOST PORT PROTOCOL SAMPLE_RATE BIT_DEPTH.
Protocols: auto, ap2, raop, compat. Defaults: 7000 auto 44100 16.
Pass the actual discovery port, especially for AirPort Express.

Environment inputs (PowerShell $env:NAME = 'value'):
- MSA_TEST_TXT: complete observed discovery TXT, space-separated key=value fields.
- MSA_TEST_MODEL: observed receiver model; do not fabricate capabilities.
- MSA_TEST_TIMING: ptp or ntp; absent uses the pinned route policy.
- MSA_TEST_BUFFERED: 1 or 0; actual selected lane follows source policy.
- MSA_TEST_CREDENTIALS: stored HAP credentials in the engine's existing hex format.
- MSA_TEST_PASSWORD / MSA_TEST_RAOP_SECRET: existing receiver authentication.
- MSA_TEST_CN / MSA_TEST_PK / MSA_TEST_PW: actual RAOP discovery values.
- CLIAIRPLAY_MRP_TYPE130=1: opt-in only, default OFF.

Credentials are not printed by the harness. CONNECT errors show class/status/route.

Commands:
- pause / play: content gate, persistent session.
- flush: FLUSH drain barrier then START/resume continuity.
- standby then start: park and restart on the same client.
- stop then start: content stop and restart.
- progress ELAPSED_SECONDS DURATION_SECONDS
- artwork PATH: JPEG or PNG file; path may include spaces.
- quit: worker shutdown and transport teardown.
Remote commands are logged for observation; they are not automatically executed.

First prove HomePod 16/44.1 realtime and AirPort route-selected playback.
Then test advertised 24-bit/48 kHz, pause/play, FLUSH, standby/start, stop/start,
metadata/artwork/progress, remote events and a long feedback session.
Observe audible output separately and retain the console log.
Forced loss/RTX and recovery require a controlled follow-up test; this harness
does not inject packet loss or claim those cases passed.
STATUS currently exposes native media/clock diagnostics; RAOP media counters
are unavailable and print None. Hard control failure exits nonzero.
