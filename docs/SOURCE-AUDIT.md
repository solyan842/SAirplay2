# SAirplay2 source parity audit

Primary reference pinned for this audit:

- music-assistant/airplay-cli
- commit `431c5c582eef9307c4e39c50a0ea65e970bc1128`
- primary files: `src/ap2_client.c`, `src/ap2_ptp.c`, `src/ap2_hap.c`, `src/ap2_io.c`, `DESIGN.md`

Rule: when the primary source already defines native AirPlay 2 behavior, SAirplay2 ports that behavior to Rust/Windows without inventing a different protocol design. Platform substitutions are allowed only below the wire/lifecycle contract.

## Audit matrix

| Area | Primary source | SAirplay2 at audit | Status / action |
|---|---|---|---|
| Discovery feature bits | AP2 bits 38/40/41/46/48, flags, model | Parsed and preserved | MATCH |
| Route selection | Conservative native/compat split | Conservative resolver | MATCH for current alpha |
| GET /info connection | Same TCP continues into HAP | Same TCP retained | MATCH |
| GET /info CSeq | Main RTSP counter starts at 0 | CSeq 0 | MATCH |
| RTSP User-Agent | `AirPlay/670.6.2` | Same | MATCH |
| RTSP base headers | CSeq, User-Agent, DACP-ID, Active-Remote | Also emits Client-Instance | **DEVIATION: remove Client-Instance from native source path** |
| /info capability parse | audioStream/bufferStream masks/extended tables | Implemented | MATCH for 16/44.1 realtime |
| HAP transient pairing | transient M1/M3, fixed PIN unless password, same TCP | Implemented | MATCH current transient path |
| Stored pair-verify | Supported by source | Not implemented | FEATURE GAP; not required for current transient HomePod test path |
| Audio key | first 32 bytes of transient SRP session key | Same | MATCH |
| Timing choice | feature bit 41 -> PTP; fallback NTP only if PTP unavailable | Same high-level decision | MATCH |
| PTP bind | UDP 319/320, multicast membership/interface + unicast peer delivery | binds 319/320 but does not join multicast | **DEVIATION** |
| PTP pre-SETPEERS delivery | peer list empty -> multicast fallback | sends unicast to receiver immediately | **DEVIATION** |
| PTP Announce/Sync/Follow_Up | source-shaped gPTP majorSdoId=1 | Implemented minimal sender | PARTIAL |
| PTP Delay/Pdelay replies | required | Implemented | MATCH basic wire path |
| PTP Signaling grants | REQUEST_UNICAST -> GRANT | Implemented | MATCH basic wire path |
| PTP BMCA / peer Announce processing | implemented; sender holds GM normally | not implemented | **GAP** |
| HomePod OS27 follow-clock | conditional AudioAccessory standalone follow mode | not implemented / TXT not passed into session | **GAP; required only when source predicate matches** |
| PTP settle | source observes peer/offset up to 400ms | fixed 400ms sleep | **DEVIATION** |
| PTP Session SETUP plist | PTP protocol, IDs, group, timingPeerInfo/List | implemented | MATCH for GM path |
| PTP identity | deviceID/macAddress/ClockID derived from DACP ID | same | MATCH |
| NTP Session SETUP | deviceID, UUID, timingPort, timingProtocol=NTP | implemented | MATCH |
| Event channel | separate TCP to eventPort, event keys, keep open | implemented and kept open | MATCH for no-MRP audio path |
| RECORD order | RECORD before stream SETUP | same | MATCH |
| Realtime Stream SETUP | type 96, ALAC, ports, latency fields, shk, spf=352 | same request fields | MATCH request |
| Stream response ports | parse dataPort/controlPort by key | same | MATCH |
| Stream response latency | parse latencyMin/latencyMax, clamp lead; arrival latency info | ignored | **DEVIATION** |
| SETPEERS | after stream SETUP for PTP; bare plist [receiver, us] | absent | **CRITICAL DEVIATION** |
| PTP peer list after SETPEERS | hand same peers to PTP engine and kick | absent | **CRITICAL DEVIATION** |
| Main RTSP CSeq | /info=0, setup=1, RECORD=2, stream=3, SETPEERS=4, then feedback=5... | PTP currently feedback starts at 4 | **DEVIATION caused by missing SETPEERS** |
| RTP SSRC | PTP=0, NTP=session_id | same | MATCH |
| RTP header | PT 96, marker first packet | same | MATCH |
| Realtime audio crypto | ChaCha20-Poly1305, seq nonce, AAD timestamp+SSRC, nonce suffix | same | MATCH |
| NTP sync | 20-byte D4 | implemented | MATCH |
| PTP realtime anchor | 28-byte D7, PTP ns + ClockID + frame geometry | implemented after anchor fix | MATCH for GM clock; follow-clock gap remains |
| ALAC 16/44.1/352 | fixed realtime encoder | implemented | MATCH target format |
| Delivery pacing window | latencyMax-250ms or default 1.75s; splice depth rules | absent | **DEVIATION** |
| Initial fill spacing | >=1ms packet release spacing | absent | **DEVIATION** |
| Retransmit history | 512 exact wire packets | absent | **DEVIATION** |
| Retransmit request | read control UDP type 0x55 | absent | **DEVIATION** |
| Retransmit response | type 0x56/D6 + original wire RTP | absent | **DEVIATION** |
| /feedback keepalive | POST /feedback every ~2s | added | MATCH cadence after latest refactor |
| Feedback timeout | 2s | added 2s | MATCH |
| Feedback miss budget | 3 consecutive misses | added | MATCH basic policy |
| RTSP serializer | one shared channel + lock + global CSeq | added in latest refactor | MATCH architecture |
| Late feedback response carry | preserve stream/HAP nonce sequencing | channel preserves encrypted carry/pending stale CSeq | MATCH basic mechanism |
| MRP/event servicing | source services MRP when MRP exists | MRP not implemented | FEATURE GAP, not required for base audio |
| Native volume | SET_PARAMETER text/parameters on shared RTSP | absent | FEATURE GAP |
| Metadata | source supports native metadata/MRP | absent | FEATURE GAP; not base HomePod transport prerequisite |
| Buffered type 103 | opt-in only in source | not implemented | OK for current realtime-only target |
| Warm splice/flush lifecycle | source has persistent timeline behavior | partial standalone timeline components, not integrated into native live session | **GAP for later pause/seek/source-change acceptance tests** |
| TEARDOWN | source sends clean native teardown | socket currently drops without native TEARDOWN | **DEVIATION** |
| Feedback/retransmit shutdown ordering | workers stop before sockets/resources close | feedback ordering implemented; retransmit absent | PARTIAL |

## Correction order

Do not add unrelated features while parity corrections are open.

1. Remove non-source `Client-Instance` from native RTSP requests.
2. Port PTP `SETPEERS` exactly after realtime Stream SETUP; advance shared CSeq correctly.
3. Port PTP peer-list semantics and multicast fallback/member handling from `ap2_ptp.c`.
4. Parse stream response `latencyMin/latencyMax` and clamp effective lead exactly like source.
5. Port delivery pacing window + 1ms initial-fill spacing.
6. Port realtime retransmit ring and 0x55/0x56 responder.
7. Port clean TEARDOWN on stop/drop using the same shared RTSP lock/CSeq.
8. Pass the full relevant TXT context into native session and port the source HomePod OS27 follow-clock predicate/engine behavior before claiming OS27 parity.
9. Only after the above, integrate warm splice/flush and source-change lifecycle needed by the stable acceptance tests.

## Items intentionally not treated as current protocol defects

- Buffered type 103: source makes it opt-in (`buffered_requested`), so realtime type 96 is valid.
- MRP/type-130/now-playing: not required to establish the base realtime audio path.
- Pair-verify credentials: source supports it, but current alpha is intentionally exercising transient pairing; add when credential UI/storage is introduced.

## Stable gate

No build is called stable until the audit's wire-critical deviations are closed and hardware passes:
cold start, >60s continuous audio, silence 15/30/60s then resume, source changes, repeated next, format transitions, HomePod AP2, AirPort AP2/RAOP as applicable, and 30-minute run.
