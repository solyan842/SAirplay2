# GitHub Actions — Full Log Reading Procedure

Locked: 2026-10-04

## Purpose

Prevent future debugging from incorrectly concluding that a GitHub Actions log is missing, truncated, or unavailable when only the connector preview is incomplete.

## Mandatory procedure

For any GitHub Actions failure investigation, use this sequence:

1. Resolve the workflow **run ID**.
2. Resolve the workflow **job ID** for the failed job.
3. Fetch the decoded log for that **job ID**.
4. If the connector returns a `Resource uri`, treat that resource as the canonical full decoded job log.
5. Read/search the resource in pages or by exact error text until the relevant beginning, failure point, and end of the job are verified.
6. Do **not** conclude that the log is incomplete merely because the first tool response shows only a preview.

Canonical flow:

```text
run ID -> job ID -> decoded job log -> Resource uri -> paged read/search
```

## Do not use as proof of missing logs

The workflow-run archive endpoint:

```text
/actions/runs/{run_id}/logs
```

may return an empty exposed `content` field through the connector even when the individual job log is available. Therefore an empty response from that endpoint is **not** proof that the Action log cannot be read.

Likewise, a truncated-looking tool preview is only a preview unless the underlying resource itself has been exhausted.

## Verification case

Windows Action **#1414** on `dev/msa-core-architecture` verified this procedure.

- commit: `2fc9360e25f6781d5be1a9b1359f1179097e0cbe`
- workflow run ID: `37212625732`
- failed build job ID: `111466710010`
- decoded job log was readable from the runner-provisioning/startup section through the compile failure.
- the actual failure was found inside the full resource:

```text
pthread.h(72): fatal error C1083: Cannot open include file: '_ptw32.h': No such file or directory
```

The later wrapper message:

```text
Pinned libraop client x64 compile failed
```

is secondary; future debugging must inspect the preceding compiler error from the full job resource.

## Debugging invariant

**Never diagnose an Action from metadata, a screenshot, a wrapper error, or the initial log preview when a decoded job-log resource is available. Read/search the full resource first.**

This is a tooling/debugging rule only. It does not change runtime, transport, MSA, RAOP, AirPlay 2, GUI, or stable code.
