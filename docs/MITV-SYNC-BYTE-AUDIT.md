# MiTV SYNC byte audit

Branch: `compat/mitv-happycast-raop-diag`. Baseline: #22,
`edea68ce12aa4d8f26be9208c50656cd75a7e065`.

#23 added observations only to that baseline. The next build corrects only the
MiTV wire NTP epoch as described below. Audible success remains unverified.
Stable, native AP2, SOtM base worker and capture are unchanged.

## Hardware evidence from #23 and scoped correction

The user's #23 log contains actual initial SYNC bytes
`90d40007561d9085a49bb3bd459e0000561ee90d`. They decode to NTP
`2761667517.1167982592`, while the simultaneously sampled Unix clock is
`1793107005283 ms` and RFC/NTP seconds are `4002095805`. The initial head
and sampled wall clock differ by approximately 11 ms, not by an epoch.
Periodic SYNC shows the same mismatch: `2761667526` versus `4002095814`.

Subtracting `11644473600` seconds (FILETIME origin to Unix origin) modulo
2^32 maps both recorded values exactly to their RFC/NTP seconds. This is
consistent with pinned Windows crosstools adding the NTP offset to FILETIME
without first removing FILETIME's origin. Both RTP differences remain valid:
initial SYNC head minus first audio is 352; SYNC head minus latency RTP is 88200.

The correction applies that subtraction only when serializing SYNC NTP seconds
and the timing response receive/send seconds for port 52266. The timing response
reference field still echoes the receiver's original bytes. Fractions, RTP
timestamps, START/RECORD, latency, codecs and internal scheduling clocks remain
unchanged. The response send timestamp remains the pinned copy of receive time.
There is no additional Unix-to-NTP offset: the private Windows value already
contains that addition. Other ports return the original seconds unchanged.

This is an evidenced wire clock defect, not proof that it caused the HappyCast
crash or the only cause of silence. The new mDNS hostname plus failed feedback
are consistent with a receiver service interruption but are not a crash trace.
The log also records control_req=1/retransmit=1, so the previous observation of
no receiver control traffic no longer holds for this session.

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
  measure. #23 has now measured it; the correction above stays at the wire edge.

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
- Source gate hashes the #22 SYNC function after removing telemetry and
  normalizing the one documented seconds conversion, preventing other drift.
- A compiled C regression checks both hardware clock vectors, era rollover,
  and unchanged seconds for all 65535 other ports. Its output and exit status
  are retained in the durable build log.
- Existing Windows MSA tests and GUI/DLL builds remain required. Packaged
  GUI/DLL must contain the new diagnostic strings and include BUILD-INFO.txt.

The next device run has one purpose: test corrected wire NTP seconds while
retaining the measured RTP mapping. Run music for at least 15 seconds and retain
WIRE plus SYNC lines, noting audible output and whether HappyCast stays open.
No change to codec, latency, RECORD, first-SYNC ordering or User-Agent is part
of this build. PCM amplitude and network capture remain separate follow-ups
if the measured SYNC mapping is valid.
