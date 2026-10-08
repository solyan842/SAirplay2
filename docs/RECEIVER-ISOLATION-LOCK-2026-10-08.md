# SAirplay2 — Receiver Isolation Lock

Date: 2026-10-08
Status: PROJECT RULE / DO NOT DRIFT

## Core principle

A receiver-specific failure must be investigated and fixed in that receiver's own compatibility lane. Do not make a known-good, hardware-locked receiver follow the behavior, diagnostics, timing, lifecycle, or protocol experiments of a failing receiver.

## SOtM baseline

SOtM RAOP 16-bit lifecycle at commit `c64e40dc56d7fe128aaae25846600715acbfef91` is HARDWARE-LOCKED / FIELD-PASS and is the recovery reference.

Do not modify or retune the SOtM baseline to investigate MiTV or any other receiver. This includes START, FLUSH, NTP, ALAC, packet pacing, reservoir, latency, lifecycle rules, or receiver-specific diagnostics.

A comparison against SOtM is allowed only as passive evidence when it directly distinguishes a hypothesis. The comparison itself is never a reason to alter SOtM or make SOtM enter a MiTV-specific path.

## MiTV / HappyCast rule

MiTV / Xiaomi SmartShare / HappyCast compatibility work must remain isolated from the locked SOtM path and from generic MSA behavior unless evidence proves a general defect.

Do not promote a MiTV workaround into shared behavior merely because it makes MiTV progress further. First prove that the change is required by the relevant protocol or by upstream MSA/libraop behavior for the affected receiver class.

Do not classify or route solely from an advertised Apple model such as `AppleTV3,1` or `AppleTV3,2`; third-party receivers can impersonate those identities.

## Decision discipline

Before every transport change, answer these questions in order:

1. Which receiver and exact failure is being fixed?
2. Is the affected path already hardware-locked on another receiver?
3. Is there direct log/wire/source evidence identifying the failing protocol layer?
4. Does pinned MSA/libraop already define the behavior?
5. Can the change be isolated to the failing receiver's compatibility lane?

If the proposed change contradicts an existing project lock or requires a good receiver to follow a failing receiver's experiment, stop. The proposal is invalid even if it would make testing easier.

## Working rule

One task -> one result -> stop. Do not broaden scope, retune unrelated transports, or use convenience diagnostics that contaminate a locked baseline.
