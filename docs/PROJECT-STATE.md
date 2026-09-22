# SAirplay2 — Project State

Last updated: 2026-09-22  
Working branch: `dev/discovery-foundation`

## 1. Repository / branch policy

Repository: `solyan842/SAirplay2`

Locked stable commit:

```
a7cb24b1faa6b54abf7d24812b732ed08eb72524
```

**Do not modify stable.**

Current development head when this file was created:

```
13c39374c03fe475a0749144c5c37c561d38d156
fix: protect AppleTV-class RAOP receivers and teardown gracefully
```

Working rules:

- Compare against upstream/source references before changing protocol/audio behavior.
- Do not invent a new mechanism when the referenced source already defines one.
- Keep patches minimal and isolated to the failing path.
- Do not regress HomePod native, MultiRoom, Stereo Pair, or already-stable behavior.
- Build with GitHub Actions after code changes; runtime testing starts only after CI passes.
- Preserve known-good teardown behavior for legacy helpers.
- Work in short steps: one change -> verify -> report -> stop.

## 2. Source references

Pinned references currently used for comparison:

- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`
- `music-assistant/server@9e311eb84aba0a940bdfbf7433d5a29c07bab1b6`
- `philippe44/libraop@81c2182649da8645ac2a58b78e9f370c79a4165b`

Do not silently replace these references while diagnosing the current legacy-TV issue. If a newer upstream commit is intentionally adopted, record the exact SHA and reason here.

## 3. Confirmed working behavior

The following behavior has been confirmed working and must be protected:

- HomePod native playback.
- HomePod Stereo Pair.
- MultiRoom with two HomePods.
- GUI playback modes are separated as:
  - `PlaybackMode::Single`
  - `PlaybackMode::MultiRoom`
  - `PlaybackMode::StereoPair`
- Stereo Pair status is no longer conflated with MultiRoom.
- First-attempt `/feedback ... peer/control channel closed` for Pair/MultiRoom has a single automatic retry.
- Legacy `cliraop` Stop/Exit no longer leaves the app permanently hung.

Important historical teardown fix:

```
f6c885b3b203a2e0a9f9818db7bbbe4cab4f7244
```

That fix established the anti-hang requirement and must not be lost.

## 4. Current teardown behavior

Current development head `13c3937...` refined the legacy shutdown path:

- Prefer normal stdin EOF -> drain -> `raopcl_disconnect()` / destroy.
- Allow the helper a short graceful-exit window.
- Keep a delayed watchdog so a blocked helper still cannot hang shutdown indefinitely.
- The legacy writer closes stdin before waiting for the helper.
- A stuck helper is still force-terminated as fallback.

This is intended to preserve the anti-hang guarantee while avoiding unnecessarily abrupt receiver teardown.

## 5. Cleanup already completed

Cleanup commit:

```
9da993c3135ac836cc8f9f5502a20dad0459e862
```

Removed diagnostic/prototype noise:

- Companion/MRP test discovery.
- Unneeded feature/flag/PTP/buffered dumps.
- Long WASAPI diagnostics.
- Long RTX diagnostics.
- Native preflight diagnostics.
- `et/md/am/pk` debug dumps after the relevant investigation.

Keep only operationally useful logging:

- discovered devices,
- connect,
- playback,
- transport/audio/control errors,
- disconnect/stall.

## 6. Test receiver: embedded legacy RAOP TV

Device label:

```
Mi Project - Living room
```

Observed receiver data:

- IP: `192.168.88.15`
- model: `AppleTV3,1`
- RAOP port: `52266`
- AirPlay/RAOP have appeared on the same advertised port.
- previously observed features: `0x0000001E527FFFF7`
- flags: `0x4`
- `et=0,3,5`
- `md=0,1,2`
- `pk`: present
- no PIN-required `0x8`
- no legacy-pairing `0x200`
- receiver exposes no usable PIN UI.
- testing PIN `3939` had no effect.

Do **not** infer that AppleTV legacy PIN pairing is valid merely from `model=AppleTV*` plus `pk`.

An iPhone comparison also showed that another Apple TV in the home does not appear in the audio AirPlay picker. Therefore Companion/MRP discovery is not part of the current audio-receiver problem.

## 7. Legacy RAOP timeline

### Build #588

Observed:

- RTSP/RAOP connection succeeds.
- `connected to 192.168.88.15 on port 52266`
- player latency reported as `1250 ms`
- no audible output.
- later saw:
  `libraop writer queue overrun; removed instead of dropping PCM.`

### Build #589

Attempt:

- increased writer queue/headroom.

Result:

- did not restore audio.
- Stop/Exit could still leave `cliraop` stuck.

### Source comparison after #589

Conclusion:

- do not use a custom 12-second queue.
- use backpressure.
- only consider the writer dead after a long period in which it does not consume PCM.

### Build #594

Teardown fix:

- ensure a blocked `cliraop` cannot hold Rust workers/GUI shutdown indefinitely.

Runtime result confirmed by user:

- Stop no longer hangs the application.
- `cliraop.exe` no longer remains stuck in Task Manager.

### Build #595

Commit:

```
942ff8f16a828cb069c340234e95f8b89a38d648
```

Change:

- for a single legacy RAOP receiver, removed the explicit future NTP start argument.
- did not use the shared group start anchor for that single receiver.
- left pacing to `cliraop/libraop` through `raopcl_accept_frames()`.
- legacy groups with more than one receiver retained their shared start anchor.

Runtime result:

- TV still produced no audio.
- worse, after playback attempts the receiver's AirPlay service could temporarily disappear from discovery and require rescanning/recovery.

Observed example:

```
mDNS RAOP: Mi Project - Living room @ MITV--2028176950.local.:52266
mDNS AirPlay: Mi Project - Living room @ MITV--2028176950.local.:52266 · model=AppleTV3,1

Mi Project - Living room: transport Ready, Windows audio running on 1 receiver(s).
Mi Project - Living room: ... using ALAC coding
Mi Project - Living room: ... local interface 192.168.88.148
Mi Project - Living room: ... setting volume as part of connect -15.00
Mi Project - Living room: ... connected to 192.168.88.15 on port 52266, player latency is 1250 ms

Playback stopped; AirPlay session resources released.

mDNS AirPlay: Mi Project - Living room @ MITV--1985685046.local.:52266 · model=AppleTV3,1
mDNS RAOP: Mi Project - Living room @ MITV--1985685046.local.:52266
```

The changing `MITV--...` instance after the session is an observed symptom, not by itself proof of the root cause.

## 8. Current protection at development head

Commit `13c39374c03fe475a0749144c5c37c561d38d156` added a fail-closed guard for AppleTV-class legacy RAOP receivers that advertise `pk` but have no stored pairing secret.

For the current embedded TV, this is a **receiver-protection measure while the protocol path is being verified**. It should not be treated as proof that pairing is the final root cause of the no-audio issue.

The same commit also changed the native idle device badge to:

```
Sẵn sàng / Ready
```

instead of using `Waiting` for an available idle receiver.

## 9. Confirmed libraop behavior relevant to the investigation

From the pinned `libraop` source:

- `raopcl_connect()` performs RTSP connect -> SDP -> ANNOUNCE -> SETUP -> RECORD -> volume handling.
- after successful connect, state is `RAOP_FLUSHED`.
- `raopcl_accept_frames()` transitions playback to `RAOP_STREAMING`.
- `_raopcl_send_audio()` sends only when the audio fd is valid and state is `RAOP_STREAMING`.

Therefore a successful `raopcl_connect()` log proves RTSP setup progressed, but does not by itself prove audible audio packets were accepted/rendered by the receiver.

## 10. Next investigation — do this before another legacy-TV transport patch

Do not continue with trial-and-error transport changes.

First compare these three areas exactly:

1. **SAirplay2 -> helper command line**
   - Capture/inspect the exact arguments currently passed to `cliraop`.
   - Compare them field-by-field with the referenced `cliraop` / current `cliairplay` source contract.
   - Pay attention to codec, latency/start timing, credentials, encryption and receiver TXT-derived options.

2. **Encryption**
   - Music Assistant has `CONF_ENCRYPTION`.
   - Its general RAOP configuration may default encryption on at a higher configuration layer.
   - `airplay-cli` uses `--encrypt` based on receiver capabilities/selected path.
   - This TV advertises `et=0,3,5`, not `1`.
   - Determine from source what the correct behavior for this exact capability set is.
   - Do not force encryption merely to test a theory.

3. **Legacy helper choice**
   - Determine whether keeping old `cliraop` is correct for this receiver or whether the newer unified `cliairplay` path is required.
   - Before considering migration, verify Windows build/runtime feasibility.
   - Any experiment must be isolated to the legacy TV path.
   - Do not replace or disturb the HomePod/AirPlay 2 native engine.
   - Do not redesign the whole architecture to solve one receiver.

Also trace the point where PCM reaches helper stdin and where `raopcl_accept_frames()` begins returning/accepting frames. The key unresolved question is still: **RTSP setup succeeds, but why is no audible stream produced, and why did the receiver service restart/disappear after some sessions?**

## 11. GUI status wording

Target status vocabulary:

- idle available receiver -> `Sẵn sàng / Ready`
- connecting -> `Đang kết nối / Connecting`
- playing -> `Đang chạy / Running`
- error -> `Lỗi kết nối / Connection Error`

Use `Đang chờ phát nhạc... / Waiting for playback...` only for the overall playback description, not as the normal per-device idle badge.

At current head, the native idle badge has already been moved to `Sẵn sàng / Ready`. Audit the remaining routes later, separately from the legacy-TV transport investigation.

## 12. Handoff rule

For a new conversation, start with:

> Continue SAirplay2. Read `docs/PROJECT-STATE.md` from branch `dev/discovery-foundation` and continue from the documented next investigation.

When a new stable milestone is confirmed by runtime testing, update this file with:

- exact commit SHA,
- CI result,
- what was tested,
- what is confirmed working,
- remaining issue,
- next single action.


## 13. Command-line audit — 2026-09-22

Compared directly against:

- `philippe44/libraop@81c2182649da8645ac2a58b78e9f370c79a4165b` — `src/cliraop.c`
- `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128` — `src/cliairplay.c`
- `music-assistant/server@9e311eb84aba0a940bdfbf7433d5a29c07bab1b6` — AirPlay `stream.py`

Current SAirplay2 legacy helper invocation is structurally:

```
cliraop
  -p <port>
  -v <0..100>
  -l 44100
  -t <et>
  -m <md>
  -d 3
  [-n <start_ntp>]
  -a
  [-u]
  [-s <secret>]
  <host>
  -
```

Findings:

1. **The current arguments are valid for the pinned old `cliraop`.**
   - `-p/-v/-l/-t/-m/-d/-n/-a/-u/-s` all map directly to the pinned source.
   - stdin `-` is the expected PCM input path.
   - `-a` selects compressed ALAC in the pinned helper.

2. **Encryption is not the current TV mismatch.**
   - Music Assistant appends `--encrypt` by default for a RAOP target when its encryption config is enabled.
   - However current `cliairplay` only selects `RAOP_RSA` when both `--encrypt` is present **and** the receiver `et` contains `1`.
   - The test TV advertises `et=0,3,5`.
   - Therefore current `cliairplay` would still use `RAOP_CLEAR` for this TV.
   - SAirplay2 currently also uses clear RAOP because it does not pass old `cliraop -e`.
   - Do **not** add `-e` as a speculative fix for this receiver.

3. **The major behavioral difference is the newer source's AppleTV guard.**
   - Current `cliairplay::run_raop()` refuses RAOP when:
     - `am` contains `AppleTV`,
     - `pk` is present,
     - and no stored `secret` exists.
   - SAirplay2 development head `13c3937...` copied this fail-closed rule before spawning old `cliraop`.
   - The embedded TV under test advertises `AppleTV3,1` + `pk` but exposes no usable PIN UI.
   - Therefore matching the newer generic guard is source-consistent, but it is **not proof that this particular embedded TV actually requires legacy AppleTV pairing**.
   - This guard must not be used to justify forcing PIN pairing on this receiver.

4. **Old `cliraop` itself does not consume `am/pk/pw/cn`.**
   - It receives `et`, `md`, optional auth/secret/password/encryption and codec choice.
   - The newer unified `cliairplay` accepts the richer discovery fields and owns route/auth/codec decisions.
   - This is an architectural difference to evaluate separately; it does not justify changing the HomePod/native engine.

Result of this audit:

- No transport code was changed.
- No encryption patch is warranted.
- The next single investigation should be whether the unified `cliairplay` RAOP path can be built and run on Windows in isolation for this legacy-TV route, and whether its AppleTV guard must be adapted for embedded receivers before any migration is attempted.


## 14. Unified `cliairplay` Windows feasibility audit — 2026-09-22

Compared against `music-assistant/airplay-cli@431c5c582eef9307c4e39c50a0ea65e970bc1128`.

Finding: **the current upstream unified `cliairplay` is not a drop-in Windows helper.**

Evidence from the pinned source/build system:

- The upstream GitHub Actions matrix builds only:
  - Linux x86_64,
  - Linux aarch64,
  - macOS arm64,
  - macOS x86_64.
- Release assets likewise contain only those Linux/macOS binaries; there is no Windows artifact.
- The Makefile's native host detection only handles Darwin vs Linux.
- The executable links against POSIX-oriented facilities such as `pthread`, `dl`, and on Linux `rt`.
- `cliairplay.c` directly includes/uses POSIX APIs including:
  - `unistd.h`,
  - `poll.h`,
  - POSIX file/stat APIs,
  - `mkfifo()` for the command pipe,
  - pthread-based synchronization/threading.
- The unified process contract requires a POSIX-style `--cmdpipe` FIFO in addition to PCM on stdin.

Implication for SAirplay2:

- Replacing the existing Windows `cliraop.exe` with upstream `cliairplay` would **not** be a minimal helper swap.
- It would require a real Windows port/adaptation layer for process control, command pipe IPC, polling/thread primitives, and likely build/dependency packaging.
- Such a port would materially widen the scope and create regression risk for a problem currently isolated to one legacy TV receiver.
- Therefore do **not** migrate the legacy TV path to `cliairplay` at this stage.
- Preserve the current Windows `cliraop` helper and continue diagnosis at the RAOP/libraop behavior level.

Next single investigation:

- Trace the pinned old `cliraop/libraop` path from stdin consumption through `raopcl_accept_frames()` to `raopcl_send_chunk()`, and compare the exact state/timing conditions with the TV session that connects successfully but remains silent.
- Prefer instrumentation or source-proven checks that are isolated to the legacy-TV path.
- Do not alter HomePod/native, MultiRoom, Stereo Pair, or the stable branch.
