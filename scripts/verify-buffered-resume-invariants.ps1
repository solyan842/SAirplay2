$ErrorActionPreference = "Stop"

$worker = "crates/sairplay-msa-solo/src/windows_audio_worker.rs"
if (-not (Test-Path $worker)) {
    throw "Missing worker source: $worker"
}

$text = [System.IO.File]::ReadAllText($worker)

function Require-Contains([string]$Needle, [string]$Label) {
    if (-not $text.Contains($Needle)) {
        throw "Buffered resume invariant missing: $Label"
    }
}

function Require-Absent([string]$Needle, [string]$Label) {
    if ($text.Contains($Needle)) {
        throw "Buffered resume regression present: $Label"
    }
}

# Field-proven native AP2 Buffered resume path (Naim Mu-so Qb, 44.1/16):
# capture idle -> STANDBY/FLUSHBUFFERED -> local PCM flush -> fresh PCM ->
# deferred START with negotiated receiver lead. This lock is scoped to the
# Buffered Type-103 Windows-adapter lifecycle; it is not a global 16-bit baseline.
Require-Contains 'let mut buffered_capture_parked = false;' 'parked-state owner'
Require-Contains 'buffered_park_flush_target: Option<u64> = None;' 'flush-generation gate'
Require-Contains 'guard.standby().map(|_| true);' 'protocol-native STANDBY boundary'
Require-Contains 'STANDBY + FLUSHBUFFERED completed' 'field-visible standby/flush telemetry'
Require-Contains 'buffered_park_flush_target.is_none()' 'resume waits for local flush acknowledgement'
Require-Contains 'fresh post-flush PCM detected; deferred Buffered START re-armed' 'fresh-PCM resume gate'
Require-Contains 'guard.effective_lead_ms().max(DEFERRED_START_LEAD_MS);' 'receiver-derived deferred START lead'
Require-Contains 'now_unix_ms.saturating_add(receiver_lead_ms);' 'receiver-derived START anchor'

# The failed in-place inferred PAUSE/PLAY path must not silently return.
Require-Absent 'rate-1 anchor restored without FLUSHBUFFERED' 'old inferred rate-1 resume path'
Require-Absent 'let mut buffered_capture_paused = false;' 'old inferred pause state'

Write-Host "Buffered resume invariants verified: STANDBY/FLUSH + fresh PCM + receiver lead."
