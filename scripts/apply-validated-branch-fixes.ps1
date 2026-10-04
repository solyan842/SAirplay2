$ErrorActionPreference = "Stop"

function Replace-Exact([string]$Path, [string]$Old, [string]$New, [int]$ExpectedCount = 1) {
    $text = [System.IO.File]::ReadAllText($Path)
    $count = ([regex]::Matches($text, [regex]::Escape($Old))).Count
    if ($count -ne $ExpectedCount) {
        throw "${Path}: expected $ExpectedCount exact match(es), found $count"
    }
    $text = $text.Replace($Old, $New)
    [System.IO.File]::WriteAllText($Path, $text, [System.Text.UTF8Encoding]::new($false))
}

$gui = "crates/sairplay-gui/src/main.rs"
# GUI top control cards: frame inner margins are 10px per side = 20px total.
Replace-Exact $gui '        const CARD_HORIZONTAL_MARGIN: f32 = 16.0;' '        const CARD_HORIZONTAL_MARGIN: f32 = 20.0;' 1

# GUI volume card: left_to_right layout also inserts the theme item spacing between
# the fixed 54px speaker column, explicit 4px gap, and the volume controls.
# Account for that automatic spacing so the first card cannot grow into card #2.
Replace-Exact $gui '                            let volume_controls_w = (card_inner_w - 58.0).max(100.0);' '                            let volume_controls_w = (card_inner_w - 58.0 - ui.spacing().item_spacing.x).max(100.0);' 1

# GUI: force a visible gutter between the two top device panels.
Replace-Exact $gui @'
                ui.columns(2, |columns| {
'@ @'
                ui.spacing_mut().item_spacing.x = 14.0;
                ui.columns(2, |columns| {
'@ 1

# GUI: helper text returns to neutral gray, regular (non-italic) style.
Replace-Exact $gui @'
                            egui::RichText::new(hint)
                                .size(11.4)
                                .italics()
                                .color(UiTheme::blue()),
'@ @'
                            egui::RichText::new(hint)
                                .size(11.4)
                                .color(UiTheme::text_soft()),
'@ 1

# Temporary HomePod Type-96 diagnostic instrumentation only. This changes log
# visibility, not AP2 lifecycle/timing/media behavior. Keep the committed Rust
# source as the transport source of truth until the intermittent resume failure
# has one good and one bad boundary capture to compare.
$worker = "crates/sairplay-msa-solo/src/windows_audio_worker.rs"

Replace-Exact $worker @'
                    let mut starvation_started: Option<Instant> = None;
                    let mut last_starvation_recovery: Option<Instant> = None;
                    let mut deferred_audio_seen: Option<Instant> = None;
'@ @'
                    let mut starvation_started: Option<Instant> = None;
                    let mut last_starvation_recovery: Option<Instant> = None;
                    let mut starvation_recovery_count: u64 = 0;
                    let mut deferred_audio_seen: Option<Instant> = None;
'@ 1

Replace-Exact $worker @'
                            starvation_started = None;
                            last_starvation_recovery = None;
                            deferred_audio_seen = None;
'@ @'
                            starvation_started = None;
                            last_starvation_recovery = None;
                            starvation_recovery_count = 0;
                            deferred_audio_seen = None;
'@ 1

Replace-Exact $worker @'
                                } else {
                                    Some(guard.send_pcm_352(&packet))
                                }
'@ @'
                                } else {
                                    if starvation_started.is_some() {
                                        let diag = guard.diagnostics();
                                        let elapsed_ms = starvation_started
                                            .map(|started| started.elapsed().as_millis())
                                            .unwrap_or(0);
                                        if let Ok(mut events) = events_thread.lock() {
                                            events.push(format!(
                                                "MSA INPUT REALTIME starvation-exit BEFORE first PCM: elapsed={}ms recoveries={} capture_idle={}ms state={:?} seq={} rtp={} head_frame={} pacing_ahead_frames={} splice_pad_frames={} reanchors={} shifted_frames={} ptp_anchor_valid={} ptp_wall0_ns={} ptp_pos0={} audio_sent={} audio_dropped={} sync_sent={} sync_dropped={} capture_gen={} non_silent_gen={} flush_gen={} flush_ack={}.",
                                                elapsed_ms,
                                                starvation_recovery_count,
                                                last_capture_frame_at.elapsed().as_millis(),
                                                diag.state,
                                                diag.seq,
                                                diag.rtp,
                                                diag.head_frame,
                                                diag.pacing_ahead_frames,
                                                guard.runtime.splice_pad_frames,
                                                guard.runtime.timeline_reanchors,
                                                guard.runtime.reanchor_shifted_frames,
                                                guard.runtime.ptp_anchor.valid,
                                                guard.runtime.ptp_anchor.wall0_ns,
                                                guard.runtime.ptp_anchor.pos0,
                                                diag.audio_sent,
                                                diag.audio_dropped,
                                                diag.sync_sent,
                                                diag.sync_dropped,
                                                pcm_hub_thread.capture_frame_generation(),
                                                pcm_hub_thread.non_silent_generation(),
                                                pcm_hub_thread.flush_generation(),
                                                pcm_hub_thread.flush_ack_generation(),
                                            ));
                                        }
                                    }
                                    Some(guard.send_pcm_352(&packet))
                                }
'@ 1

Replace-Exact $worker @'
                                Some(Ok(SendResult::Sent | SendResult::Dropped)) => {
                                    if pad_frames != 0 {
                                        if let Ok(mut guard) = engine_thread.lock() {
                                            guard.runtime.take_splice_pad_frames(pad_frames);
                                        }
                                    }
                                    starvation_started = None;
                                    last_starvation_recovery = None;
                                }
'@ @'
                                Some(Ok(result @ (SendResult::Sent | SendResult::Dropped))) => {
                                    if starvation_started.is_some() {
                                        let elapsed_ms = starvation_started
                                            .map(|started| started.elapsed().as_millis())
                                            .unwrap_or(0);
                                        if let Ok(guard) = engine_thread.lock() {
                                            let diag = guard.diagnostics();
                                            if let Ok(mut events) = events_thread.lock() {
                                                events.push(format!(
                                                    "MSA INPUT REALTIME starvation-exit AFTER first PCM: result={:?} elapsed={}ms recoveries={} capture_idle={}ms state={:?} seq={} rtp={} head_frame={} pacing_ahead_frames={} splice_pad_frames={} reanchors={} shifted_frames={} ptp_anchor_valid={} ptp_wall0_ns={} ptp_pos0={} audio_sent={} audio_dropped={} sync_sent={} sync_dropped={} capture_gen={} non_silent_gen={} flush_gen={} flush_ack={}.",
                                                    result,
                                                    elapsed_ms,
                                                    starvation_recovery_count,
                                                    last_capture_frame_at.elapsed().as_millis(),
                                                    diag.state,
                                                    diag.seq,
                                                    diag.rtp,
                                                    diag.head_frame,
                                                    diag.pacing_ahead_frames,
                                                    guard.runtime.splice_pad_frames,
                                                    guard.runtime.timeline_reanchors,
                                                    guard.runtime.reanchor_shifted_frames,
                                                    guard.runtime.ptp_anchor.valid,
                                                    guard.runtime.ptp_anchor.wall0_ns,
                                                    guard.runtime.ptp_anchor.pos0,
                                                    diag.audio_sent,
                                                    diag.audio_dropped,
                                                    diag.sync_sent,
                                                    diag.sync_dropped,
                                                    pcm_hub_thread.capture_frame_generation(),
                                                    pcm_hub_thread.non_silent_generation(),
                                                    pcm_hub_thread.flush_generation(),
                                                    pcm_hub_thread.flush_ack_generation(),
                                                ));
                                            }
                                        }
                                    }
                                    if pad_frames != 0 {
                                        if let Ok(mut guard) = engine_thread.lock() {
                                            guard.runtime.take_splice_pad_frames(pad_frames);
                                        }
                                    }
                                    starvation_started = None;
                                    last_starvation_recovery = None;
                                    starvation_recovery_count = 0;
                                }
'@ 1

Replace-Exact $worker @'
                                last_starvation_recovery = Some(Instant::now());
                                if recovered {
                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(
                                            "WASAPI input starvation recovery queued timeline silence."
                                                .into(),
                                        );
                                    }
                                }
'@ @'
                                last_starvation_recovery = Some(Instant::now());
                                if recovered {
                                    starvation_recovery_count =
                                        starvation_recovery_count.saturating_add(1);
                                    if starvation_recovery_count == 1 {
                                        if let Ok(guard) = engine_thread.lock() {
                                            let diag = guard.diagnostics();
                                            if let Ok(mut events) = events_thread.lock() {
                                                events.push(format!(
                                                    "MSA INPUT REALTIME starvation BEGIN: capture_idle={}ms state={:?} seq={} rtp={} head_frame={} pacing_ahead_frames={} splice_pad_frames={} reanchors={} shifted_frames={} ptp_anchor_valid={} ptp_wall0_ns={} ptp_pos0={} audio_sent={} audio_dropped={} sync_sent={} sync_dropped={} capture_gen={} non_silent_gen={} flush_gen={} flush_ack={}.",
                                                    last_capture_frame_at.elapsed().as_millis(),
                                                    diag.state,
                                                    diag.seq,
                                                    diag.rtp,
                                                    diag.head_frame,
                                                    diag.pacing_ahead_frames,
                                                    guard.runtime.splice_pad_frames,
                                                    guard.runtime.timeline_reanchors,
                                                    guard.runtime.reanchor_shifted_frames,
                                                    guard.runtime.ptp_anchor.valid,
                                                    guard.runtime.ptp_anchor.wall0_ns,
                                                    guard.runtime.ptp_anchor.pos0,
                                                    diag.audio_sent,
                                                    diag.audio_dropped,
                                                    diag.sync_sent,
                                                    diag.sync_dropped,
                                                    pcm_hub_thread.capture_frame_generation(),
                                                    pcm_hub_thread.non_silent_generation(),
                                                    pcm_hub_thread.flush_generation(),
                                                    pcm_hub_thread.flush_ack_generation(),
                                                ));
                                            }
                                        }
                                    }
                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "WASAPI input starvation recovery queued timeline silence · count={}.",
                                            starvation_recovery_count
                                        ));
                                    }
                                }
'@ 1

Write-Host "Validated branch fixes applied."
git diff -- $gui $worker
"
    $beginIndex = $selfText.IndexOf($beginMarker, [System.StringComparison]::Ordinal)
    $endIndex = $selfText.IndexOf($endMarker, [System.StringComparison]::Ordinal)
    if ($beginIndex -lt 0 -or $endIndex -lt $beginIndex) {
        throw "one-shot self-clean markers not found"
    }
    $endIndex += $endMarker.Length
    while ($endIndex -lt $selfText.Length -and ($selfText[$endIndex] -eq "`r" -or $selfText[$endIndex] -eq "`n")) {
        $endIndex++
    }
    $cleanSelf = $selfText.Substring(0, $beginIndex).TrimEnd() + "`n" + $selfText.Substring($endIndex)
    [System.IO.File]::WriteAllText(
        $selfPath,
        $cleanSelf,
        [System.Text.UTF8Encoding]::new($false)
    )

    git config user.name "github-actions[bot]"
    git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
    git add -- `
        "crates/sairplay-gui/src/main.rs" `
        "crates/sairplay-msa-solo/src/windows_audio_worker.rs" `
        "scripts/apply-validated-branch-fixes.ps1"

    git diff --cached --check
    if ($LASTEXITCODE -ne 0) { throw "git diff --check failed" }

    git diff --cached --quiet
    if ($LASTEXITCODE -eq 0) { throw "source promotion produced no staged changes" }

    git commit -m "refactor: promote validated MSA core fixes into source"
    if ($LASTEXITCODE -ne 0) { throw "source promotion commit failed" }

    git push origin HEAD:dev/msa-core-architecture
    if ($LASTEXITCODE -ne 0) { throw "source promotion push failed" }

    Write-Host "MSA Core source promotion pushed."
}
# END ONE-SHOT MSA-CORE SOURCE PROMOTION
