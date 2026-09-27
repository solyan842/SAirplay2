# SAirplay2 licensing design

Status: design locked, Cryptlex product credentials not yet embedded.

Branch: `dev/licensing-foundation`

This document defines the licensing/trial contract before any production
credentials are added to the application.

## Provider

Use **Cryptlex LexActivator** as the licensing authority.

Do not implement a home-grown registry/clock scheme in parallel. LexActivator
already provides:

- node-locked activations;
- machine fingerprinting;
- encrypted local activation state;
- signed server responses;
- verified timed trials;
- system-clock tamper detection;
- offline local verification after activation;
- periodic background server sync;
- suspend/revoke support;
- offline activation if needed later.

SAirplay2 must treat LexActivator as the source of truth for license/trial
validity. Windows `SystemTime` may be used only to format display values after
LexActivator has validated the state; it must never decide whether a trial is
valid.

## Product model

Create exactly one Cryptlex Product:

```
SAirplay2
```

A license key belongs to the customer/license, not to one executable build.

Initial commercial model:

- node-locked perpetual license;
- 1 activation = 1 machine;
- the same key continues across normal SAirplay2 updates;
- no Standard/Pro split initially;
- no subscription initially.

If editions are introduced later, use Cryptlex feature entitlements rather than
creating one Product per edition.

## Trial policy

Trial duration:

```
3 days / 72 hours
```

Trial is keyless.

Expected flow:

1. initialize LexActivator with Product.dat + Product ID;
2. check paid license locally;
3. if no valid paid license, check local trial;
4. if no trial exists on the machine, activate the trial against Cryptlex;
5. on later starts, validate the stored trial locally;
6. use `GetTrialExpiryDate` only after `IsTrialGenuine` succeeds;
7. display remaining days as a UI projection of the validated expiry;
8. once expired, block starting a new audio session and show activation UI.

Never call `ActivateTrial` merely to refresh an existing trial. Use
`SyncTrialActivation` for an existing trial when a server refresh is required.

A reinstall must not reset the 3-day allowance. Cryptlex's server-side trial
activation/fingerprint is the authority.

## Anti-clock-tamper rule

Do not implement:

```
expiry = local_first_run_time + 72 hours
```

Do not trust:

```
SystemTime::now()
registry install timestamp
local JSON timestamp
file creation timestamp
```

for trial validity.

Clock validation remains enabled in the Cryptlex policy. Do **not** set
`allowedClockOffset = -1`, because that disables clock validation.

The exact allowed offset should be kept small enough to catch deliberate
rollback while tolerating ordinary Windows/NTP drift. Do not hard-code that
number in SAirplay2; configure it in the Cryptlex trial/license policy.

If LexActivator reports time tampering or otherwise refuses genuine validation,
SAirplay2 must not calculate an apparently longer trial from the Windows clock.

## Activation scope

For the intended “one key = one machine” Windows behavior, use:

```
PermissionFlags::LA_ALL_USERS
```

This stores one system-wide activation visible across Windows users on that
machine. It avoids asking the same PC to activate separately for each Windows
account.

Cryptlex license-template settings:

- type: `node-locked`
- allowed activations: `1`
- fingerprint matching strategy: `fuzzy`
- user locked: `false`
- allow VM activation: `false` initially
- allow container activation: `false`
- clock validation: enabled
- server sync interval: normal desktop cadence; do not make it aggressively
  frequent
- server sync grace period: choose a finite offline allowance appropriate for a
  consumer desktop app
- geolocation: may be disabled unless it is deliberately needed

The fingerprint matching policy is a server-side policy. Do not replace
LexActivator's fingerprint with a custom hardware hash unless a real hardware
problem proves it necessary.

## Paid activation flow

The activation window already present in SAirplay2 becomes real:

```
SetLicenseKey(user_key)
ActivateLicense()
```

On success:

- persist nothing custom containing the key or a fake activation token;
- let LexActivator persist its encrypted activation state;
- change the trial row to `Đã đăng ký`;
- allow playback.

On future starts:

```
IsLicenseGenuine()
```

runs before trial validation.

The first successful local validation also starts LexActivator's background
server synchronization. Server changes such as suspend/revoke are applied by
that mechanism and the registered license callback.

Never call `ActivateLicense` on every app start.

## Trial UI state

The current hard-coded text:

```
Dùng thử · còn 3 ngày
```

must be replaced with state derived from LexActivator.

Required states:

- `Registered`
- `TrialActive { expires_at, days_left }`
- `TrialExpired`
- `TrialNotStarted`
- `ClockTampered`
- `Suspended`
- `Revoked`
- `OfflineValidationError`
- `ConfigurationMissing`

Vietnamese UI:

- registered: `Đã đăng ký`
- active: `Dùng thử · còn N ngày`
- expired: `Dùng thử đã hết hạn`
- unstarted: `Bắt đầu dùng thử 3 ngày`

English UI:

- registered: `Registered`
- active: `Trial · N days left`
- expired: `Trial expired`
- unstarted: `Start 3-day trial`

No false “3 days left” text is allowed before an actual trial activation exists.

## Remaining-day display

The display counter is not the security decision.

After genuine trial validation, compute the visual count from the validated
expiry:

```
remaining_seconds = max(0, expiry - trusted/current display time)
days_left = ceil(remaining_seconds / 86400)
```

The app's permission to play is still determined by the LexActivator validation
status, not by `days_left`.

For display near expiry, never show a negative day count.

## Playback enforcement point

Licensing belongs above the audio engine.

Do not put Cryptlex calls in:

- RTP/PTP code;
- media sender;
- WASAPI worker;
- pairing;
- retransmit;
- group timeline.

The GUI/application layer decides whether a **new** playback session may start.

A license/trial transition caused by the background callback should be marshaled
to the UI thread. Do not mutate egui state directly from the LexActivator
callback thread.

For initial implementation, do not kill an already-running audio stream in the
middle of a packet/timeline operation. Apply an invalid/expired state at the
application lifecycle boundary and block the next start. A later policy can
choose a graceful stop if desired.

## Rust integration

Use the official Rust package:

```
lexactivator = "3.43"
```

Do not call the Cryptlex REST API directly from SAirplay2.

Initialization order:

```
SetProductData(Product.dat contents)
SetProductId(Product ID, LA_ALL_USERS)
SetReleaseVersion(current SAirplay2 version)
register callback as appropriate
validate paid license
validate trial
```

The Product ID and Product.dat are not passwords. Product.dat contains public
verification material intended to be embedded in the app. Admin/API JWT secrets
must **never** be embedded in SAirplay2.

The LexActivator native runtime/DLL packaging must be verified in the Windows
Actions artifact before this feature is enabled by default.

## Build/credential rule

Do not commit fake Product IDs or fake Product.dat content.

Production integration begins only after the real Cryptlex Product is created
and these two values are available:

1. Product ID
2. Product.dat contents

Until then:

- stable-2 remains untouched;
- the existing stable audio engine remains untouched;
- licensing work stays on `dev/licensing-foundation`;
- no build should pretend the trial is secure.

## Test matrix before release

Must pass all of the following:

1. fresh machine + internet -> starts one 3-day trial;
2. restart app -> same trial, no reactivation;
3. reboot Windows -> same trial;
4. change Windows user -> same machine-wide activation;
5. uninstall/reinstall -> trial does not reset;
6. delete SAirplay2 preferences -> trial does not reset;
7. roll system clock backward -> trial is not extended;
8. restore correct time -> valid state can recover according to SDK behavior;
9. offline after successful trial activation -> local verification works;
10. trial expires -> new playback is blocked;
11. valid paid key -> state becomes Registered;
12. same key on second PC with one-seat license -> activation rejected;
13. server suspend/revoke -> picked up by sync/callback;
14. minor hardware change accepted under fuzzy matching where appropriate;
15. VM activation rejected under initial policy;
16. HomePod/Naim/Stereo Pair/MultiRoom audio behavior unchanged when licensing
    state is valid.

## Next required input

Create the Cryptlex SAirplay2 Product + 3-day Trial Policy + one-seat node-locked
License Template, then supply:

- Product ID
- Product.dat contents

After those are available, wire the real LexActivator provider into the existing
activation UI and replace the hard-coded trial row.
