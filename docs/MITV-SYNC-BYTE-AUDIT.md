# MiTV SYNC byte audit

Branch: `compat/mitv-happycast-raop-diag`. Baseline: #22,
`edea68ce12aa4d8f26be9208c50656cd75a7e065`.

This build adds observations only to that baseline. Audible success remains
unverified. Stable, native AP2, SOtM base worker and capture are unchanged.

## Source evidence

- Pinned [libraop raop_client.c](https://github.com/philippe44/libraop/blob/81c2182649da8645ac2a58b78e9f370c79a4165b/src/raop_client.c):
  `_raopcl_send_sync` serializes a 20-byte packet; header is `90 d4 00 07`
  for initial SYNC and `80 d4 00 07` for periodic SYNC. Offset 4 is
  `head_ts - latency_frames`; offsets 8/12 are `TS2NTP(head_ts, rate)`;
  offset 16 is `head_ts`, all multi-byte fields in network order.
- `raopcl_latency()` adds 11025 frames to `latency_frames`. Thus a total
  2250 ms at 44100 Hz corresponds to 99225 frames, while the SYNC RTP
  difference is 88200 frames (2000 ms). These are distinct upstream fields.
- #22 sends first SYNC after sending audio and after incrementing `head_ts`
  by 352. An initial SYNC head of `first_audio_ts + 352` is consequently
  expected, rather than evidence that RECORD mismatches the first audio.
- Independent [OwnTone sender](https://github.com/owntone/owntone-server/blob/master/src/outputs/raop.c),
  `packets_sync_send`, likewise selects initial `0x90` and periodic `0x80`.
  This supports the header convention; it does not establish clock parity.
- Pinned crosstools submodule is `41544c653760a205cbf6cebfdeda4a9394cc6455`.
  [cross_util.c](https://github.com/philippe44/crosstools/blob/41544c653760a205cbf6cebfdeda4a9394cc6455/src/cross_util.c)
  computes Windows `gettime_us()` from FILETIME/10 plus
  `0x83AA7E80 * 1000000`, whereas POSIX uses gettimeofday directly.
  `raopcl_get_ntp()` truncates seconds to 32 bits. The Windows START adapter
  bridges relative instants, but this does not establish the epoch a receiver
  sees in the serialized SYNC. This is a concrete clock-domain concern to
  measure, not permission to change shared clocks or add a guessed epoch fix.

## New observation

`MSA RAOP DIAG SYNC` follows the existing ten-second WIRE diagnostic. It holds
the most recent initial SYNC and most recent periodic SYNC; before either is
sent its string is empty. A subsequent START can replace the initial sample.

Each sample includes the actual 20 bytes as 40 hex characters, send return,
decoded RTP/NTP fields, latency fields, rate, full internal head, private clock,
successful audio send count, current RECORD timestamp, sampled Unix wall time
and corresponding RFC/NTP seconds (modulo 2^32). Capture is after sendto and
under the existing SYNC mutex, conditional on enabled diagnostics AND port 52266.
The strings contain no audio payload. A successful send is not proof of receipt.

Use `wire` to independently decode the three timestamp fields. Compare
`rtp_head - rtp_latency` modulo 2^32 against `latency_frames`, and compare
the first head with WIRE first_audio_timestamp. Compare serialized NTP to
the private clock and sampled wall time with the head's lead/lag accounted for;
do not blindly equate a scheduled head timestamp to capture wall time.

## Gates and hardware boundary

- Exact patch anchors fail closed; all new patch commands preserve exit codes
  and append stdout/stderr to the existing always-uploaded build log.
- Compile-time assertions check SYNC size and field offsets.
- Source gate hashes the #22 SYNC function with only the observation block
  removed, preventing accidental construction/send/ordering drift.
- Existing Windows MSA tests and GUI/DLL builds remain required. Packaged
  GUI/DLL must contain the new diagnostic strings and include BUILD-INFO.txt.

The next device run has one purpose: expose the actual SYNC clock and RTP
mapping. Run music for at least 15 seconds and retain WIRE plus SYNC lines.
No change to codec, latency, RECORD, first-SYNC ordering or User-Agent is part
of this build. PCM amplitude and network capture remain separate follow-ups
if the measured SYNC mapping is valid.
