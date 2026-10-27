# Explicit MiTV native AirPlay / NTP experiment

## #25 result and next session-metadata experiment

#26 result: the receiver logs confirm et=32 and sourceVersion=670.6.2 arrive,
but SETUP still returns 500. These two fields alone do not fix classification.

Further static inspection identifies the decisive key at receiver PC 0x202d4c:
`isScreenMirroringSession`. After finding no streams, the parser looks it up.
If absent, only source versions in [366.0, 367.0) enter its session-only reply
path; modern versions fall through to stream/type validation. If present,
the parser reads the boolean and takes its session reply path. The strings,
float constants and branch instructions were read from the installed library,
not guessed from a failed RTSP status. The next adapter adds this one key with
false for audio-only operation; et/sourceVersion remain unchanged from #26.
The test checks all original fields and exactly these three added fields.

#25 reaches transient HAP Pair-Setup M3/M4 successfully. HappyCast initializes
the HAP control cipher, recognizes timingProtocol=NTP, then returns SETUP 500
with `stream check failed` and `stream type check failed`. No audio is sent.
This is distinct from the legacy ALAC decoder crash.

The successful iPhone initial SETUP logs et=32 and sourceVersion=980.77.1.
The pinned MSA NTP session plist has only deviceID/sessionUUID/timingPort/
timingProtocol. Static inspection of the installed receiver shows its session
classification checks nonzero et and a source-version gate before stream
processing. Its failing stream check tests an absent stream object; this is
consistent with a session-only SETUP entering the stream handler.

The next build adds only et=32 and sourceVersion=670.6.2 to that first plist,
through mitv_ntp_session.rs. The source version matches SAirplay2's existing
AirPlay/670.6.2 User-Agent; no iPhone name, device ID or other identity is copied.
This is a hardware-evidenced HappyCast session-classification exception, not a
pinned upstream default. It requires the launcher's exact endpoint opt-in at
the native control boundary as well as in the GUI selector. Defaults continue
to use the original four-key plist. No pairing, cipher, stream setup, RTP,
clock, pacing, volume or recovery rules are changed.

The immediate gate is whether the session SETUP becomes 200 and the subsequent
stream SETUP is reached. Audible output and crash behavior are separate gates.
Adapter tests check endpoint isolation and that only the documented keys
are added; existing NTP tests still require exactly four default keys.

Branch: compat/mitv-happycast-raop-diag. This is an alternative session test,
not a confirmed fix. The existing native control exchange is reused with a
separate opt-in MiTV metadata adapter; default requests retain the pinned
fields. No stable or SOtM code is changed.

## How to run after this build passes

1. Extract the entire Windows artifact and close any running SAirplay2.
2. Run Test-MiTV-Native-NTP.cmd, then select Mi Project - Living room only.
3. Keep ADB enabled. Retain the GUI log whether setup succeeds or fails.
4. The CONNECT line must show streaming_mode=mitv-native-ntp-test.
   READY, if reached, must show AirPlay2Native and Ntp, at 44100/16.
5. Observe audible output, whether HappyCast stays open, and Stop/Start.

The launcher sets SAIRPLAY_MITV_NATIVE_NTP_ENDPOINT=192.168.88.15:52266 only in
its child process. An optional first launcher argument replaces the IP address.
The GUI requires an exact endpoint match AND port 52266. Other receivers and
ordinary launches keep Automatic; no preference is persisted and failure does
not trigger fallback. Hi-res is disabled for this explicit test only.

The existing native path performs its normal /info and pairing, structured
SETUP, realtime media, volume and teardown. It may be rejected by HappyCast at
pairing or SETUP: successful iPhone playback does not prove support for MSA's
native pairing/encryption contract. Retain that rejection as protocol evidence;
do not bypass authentication or invent a new transport to hide it.

## Source audit and correction to the previous working hypothesis

Pinned airplay-cli 431c5c582eef9307c4e39c50a0ea65e970bc1128,
src/ap2_client.c ap2_raop_compat_connect (3090ff), calls raopcl_create and
raopcl_connect with compressed ALAC and clear transport. Thus the repository's
AirPlay2Compat use of the legacy RAOP worker is aligned with this pin. It is
NOT a missing structured-SETUP adapter. The earlier audit hypothesis that this
was a compatibility parity defect was incorrect and is superseded here.

Pinned MSA server f09136859e240fc7859160e186c2e2186e917715,
music_assistant/providers/airplay/stream.py (1036ff), maps streaming_mode into
--protocol and --timing. Pinned cliairplay.c exposes timing ntp and the explicit
force-native option; ap2_resolve_route and ap2cl_force_native govern that route.
The test selects these existing controls at the GUI configuration boundary.
Automatic's TXT-driven choice is not rewritten based on model or a failure.

Hardware comparison: iPhone at 192.168.88.216 succeeded twice using NTP,
structured SETUP, spf=352, compressionType=2, audioFormat=262144 and cn=2.
Interleaved SAirplay2 legacy sessions at 192.168.88.148 both crashed in the
receiver's audio decoder. Captured ALAC payloads decode independently, including
through the installed ARM decoder under an explicit correct configuration.
These facts justify testing the existing native route, but do not establish
the exact live-state defect in HappyCast's legacy path.

## Gates

An endpoint isolation test checks disabled/default behavior, other IPs, other
ports and wildcard rejection; it also checks that the measured feature/flag
values resolve to native/NTP with the explicit force control. CI retains its
MSA tests, GUI build, SYNC gates and durable logs. Packaging verifies that the
new selector string is present and includes this launcher and instructions.
