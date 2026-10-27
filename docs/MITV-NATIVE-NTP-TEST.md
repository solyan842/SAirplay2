# Explicit MiTV native AirPlay / NTP experiment

Branch: compat/mitv-happycast-raop-diag. This is an alternative session test,
not a confirmed fix. The previously observed native HomePod implementation is
reused without source changes. No stable or SOtM code is changed.

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
