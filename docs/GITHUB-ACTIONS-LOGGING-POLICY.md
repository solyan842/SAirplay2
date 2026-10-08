# GitHub Actions Logging Policy — LOCKED

Status: **MANDATORY / SOURCE OF TRUTH**

This policy applies to **all SAirplay2 GitHub Actions workflows**, present and future.
It exists specifically to prevent repeated failures where a workflow/job is known to fail but the raw GitHub Actions log cannot be consumed reliably by the connected tool and appears as `Skipped`, empty, truncated, or otherwise unavailable.

## 1. Durable log is mandatory

Every workflow that builds, tests, compiles, links, packages, or runs diagnostics must persist its important stdout/stderr to a file in the workspace.

Minimum rule:

- capture both stdout and stderr;
- preserve the real command exit code;
- do not convert a failing compiler/test into a false PASS;
- write enough output to identify the first meaningful/root failure;
- include native toolchains as well as Rust/.NET/etc. steps when they can fail independently.

For PowerShell/native commands, use the equivalent of:

```powershell
& <command> 2>&1 | Tee-Object -FilePath build.log -Append
$code = $LASTEXITCODE
if ($code -ne 0) { exit $code }
```

If a step must throw instead of `exit`, capture `$LASTEXITCODE` first and throw using that saved code.

## 2. Upload the log even on failure

Every durable log must be uploaded with an unconditional failure-safe artifact step:

```yaml
- name: Upload durable build log
  if: always()
  uses: actions/upload-artifact@v4
  with:
    name: <workflow>-build-log
    path: build.log
    if-no-files-found: error
```

Retention may be chosen per workflow, but the log artifact must survive failed builds long enough for diagnosis.

## 3. Diagnostic retrieval order — LOCKED

When diagnosing a failed GitHub Action:

1. identify the exact workflow run ID;
2. identify the exact failed job ID;
3. try the raw GitHub Actions job log once;
4. if the raw log is actually returned, read the real content and find the first meaningful/root error;
5. if the raw log is `Skipped`, empty, inaccessible, suppressed, truncated before the failure, or otherwise not consumable, **stop retrying that same path**;
6. immediately switch to the durable log artifact produced by this policy;
7. only after reading actual error text may code be changed.

**Never infer a compiler/build root cause from run metadata, job status, step names, or memory alone.**

## 4. No repeated connector loop

The following anti-pattern is forbidden:

`run -> jobs -> job log -> Skipped -> rediscover tools -> job log -> Skipped -> repeat`

One failed raw-log retrieval is enough to trigger the artifact fallback unless there is concrete evidence that a materially different retrieval path is now available.

Do not spend repeated attempts re-calling the same endpoint with unchanged inputs.

## 5. Workflow design rule

A new or materially modified workflow is incomplete until its durable logging path exists.

Do not wait for the first Actions failure to add logging.

At minimum, durable logging must cover every step that can be the first independent failure boundary, including where applicable:

- C/C++ compiler and linker commands;
- CMake/MSBuild/nmake;
- Rust `cargo check/test/build`;
- code generators and patch scripts whose stderr is diagnostically important;
- packaging/signing steps when they can fail independently.

## 6. SAirplay2 project locks remain authoritative

This logging policy changes **only observability and failure retrieval**. It does not authorize protocol, transport, timing, audio, SOtM, MSA Core, stable, device routing, or model-specific behavior changes.

Existing project locks in `docs/PROJECT-STATE.md` and MSA source-lock documents remain authoritative.

## 7. Current reference implementation

The MiTV workflow on branch `compat/mitv-happycast-raop-diag` implemented this policy in commit:

`f6b0e3f488fe537ec3a18245618f38751db134b7`

That workflow initializes `build.log`, appends compiler/build/test output while preserving exit codes, and uploads `MiTV-HappyCast-build-log` with `if: always()`.

Future workflows should follow the same principle, adapted to their own runner/toolchain.

## 8. Handoff rule

Any future ChatGPT/session/agent working on SAirplay2 GitHub Actions must read this file before diagnosing CI failures.

If a handoff summary conflicts with this policy, **this file wins** for Actions log retrieval behavior.
