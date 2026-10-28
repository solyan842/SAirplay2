# HappyCast realtime media release audit — 2026-10-28

Branch: `compat/mitv-happycast-raop-diag`; baseline `f290fe4e64730389591f0d3b0aa914b8d82d9d1a`.

## Source contract and measured exception

Pinned `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`, `src/ap2_client.c:166-173`, permits a 1000-us minimum release gap to fill the receiver window. The ordinary steady rate is 352 frames per packet at 44100 Hz. Keep this upstream policy for normal endpoints.

User confirmed this baseline is audible on MiTV, but quiet and stuttering. The GUI log has 58,582 sent packets, zero reported audio/sync drops; during source playback reanchors stay at 24 and recoveries at zero. Repeated `starvation-exit` text alone does not prove repeated timeline recovery. The receiver independently logs intermittent `decrypt error,ret -86` on this exact GUI session's drain thread 30007.

A 15.565-second sender capture has 1953 unique 1452-byte RTP packets, continuous sequence and +352 RTP timestamps, with matching trailing sequence nonces. 1239 consecutive arrival gaps at the sender capture point are below 2 ms. This is not proof of delivery or successful authentication on TV.

## Independent hardware differential tests

Transient HAP pairing and the same structured NTP / ALAC type-96 stream SETUP were used, without rebuilding the GUI. Each error count is filtered to the new session's exact PID/TID, excluding the still-running GUI session.

| Test | Packets | Receiver drain TID | Authentication errors |
| --- | ---: | ---: | ---: |
| High initial sequence, one packet per asyncio sleep | 400 | 30134 | 0 |
| Back-to-back groups, sequence starts zero | 400 | 30167 | 252 |
| Back-to-back groups, absolute group deadlines | 1400 | 30193 | 880 |
| Actual gap >=4.1819 ms, precise sleep | 600 | 30292 | 1 |
| Even release, actual gap >=7.2086 ms | 600 | 30317 | 0 |
| Actual gap >=6.1387 ms | 600 | 30349 | 1 |
| Actual gap >=7.2449 ms, grouped release | 600 | 30400 | 0 |

Asyncio timer requests for 2/4 ms were not treated as verified actual spacing; the subsequent tests measure `perf_counter` intervals and use precise blocking sleeps. Errors begin during media, not solely after end-of-stream. These tests identify release clustering as a reproducible trigger; they do not prove the receiver's internal memory/crypto root cause.

Raw ADB logs and packet captures stay local because logs contain unrelated private data. Local evidence is under `audit-mitv/adb/`, including `logcat-probe-*-precise.txt`, `gui-wire-summary.json`, and the actual GUI pasted log dated 2026-10-28.

## Adapter

Enable a separate socket-release pacer only when the existing explicit MiTV endpoint guard matches, timing is NTP, and format is 44100/16. Nominal absolute deadlines remain 352/44100 seconds per packet; a 7-ms minimum prevents catch-up bursts. A late wake does not accumulate ordinary relative-sleep drift. No packet bytes, crypto keys, ALAC framing, sequence/timestamp advancement, source clocks, START anchors, or pacing window constants change. The capture producer remains independent of the media worker.

The pacer is absent by default. Buffered TCP, synchronization and retransmission sockets keep their existing behavior. Existing endpoint isolation tests remain required. A pure deadline test covers cadence, normal wake jitter and a long stall without a burst; Windows CI must compile and run the full existing suite.

## Remaining hardware gates

This is a measured mitigation candidate, not audible stability PASS. The packaged Windows GUI must still be retested for sustained playback and Stop/Start. Volume is separate: GUI reports RTSP 200 for 100% while initial TV system volume is 32/100; code already sends volume after stream SETUP. A reset-order defect is not established and no volume change is included.
