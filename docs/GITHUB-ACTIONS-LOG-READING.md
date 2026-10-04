# GitHub Actions — Full Log Reading Procedure

## Purpose

Use one reliable path for GitHub Actions logs without treating `Resource uri` as a mandatory step.

## Mandatory procedure

For any GitHub Actions failure investigation:

1. Resolve the workflow **run ID**.
2. Use `fetch_workflow_run_jobs` to resolve the exact failed **job ID**.
3. Use `fetch_workflow_job_logs` for that job ID.
4. Read the returned **`result.content` directly** and verify the relevant beginning, first meaningful failure, and end of the job.
5. Only if `result.content` is incomplete **and** the connector returns a `Resource uri`, use `read_resource` / `find_in_resource` to continue reading or searching the full log.
6. If neither complete `result.content` nor a usable `Resource uri` is available, STOP and report the tooling blocker. Do not diagnose or patch from incomplete evidence.

Canonical flow:

```text
run ID -> job ID -> fetch_workflow_job_logs -> result.content
```

Fallback only when needed:

```text
incomplete result.content + Resource uri -> read_resource / find_in_resource
```

## Do not

- Do not require a `Resource uri` when `result.content` already contains the full decoded job log.
- Do not repeatedly rediscover tools instead of calling the required job-log function directly.
- Do not treat the workflow-run archive endpoint `/actions/runs/{run_id}/logs` as proof that an individual job log is unavailable.
- Do not diagnose an Action from metadata, a wrapper error, or a screenshot when the full decoded job log is available.

## Debugging invariant

**Read `result.content` first. `Resource uri` is a fallback, not a prerequisite.**

This is a tooling/debugging rule only. It does not change runtime, transport, MSA, RAOP, AirPlay 2, GUI, or stable code.
