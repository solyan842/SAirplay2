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

$worker = "crates/sairplay-msa-solo/src/windows_audio_worker.rs"
$solo = "crates/sairplay-msa-solo/src/native_solo.rs"
$gui = "crates/sairplay-gui/src/main.rs"

# CI #1354 compile fix only: Pcm352Chunker exposes has_packet(), not fill().
Replace-Exact $worker @'
                                    let ring_fill = ring_thread
                                        .lock()
                                        .map(|ring| ring.fill())
                                        .unwrap_or(0);
'@ @'
                                    let ring_has_packet = ring_thread
                                        .lock()
                                        .map(|ring| ring.has_packet())
                                        .unwrap_or(false);
'@ 2
Replace-Exact $worker 'ring_fill={ring_fill}' 'ring_has_packet={ring_has_packet}' 2

# control_healthy() needs mutable access to the native session guard.
Replace-Exact $worker '.map(|guard| guard.control_healthy())' '.map(|mut guard| guard.control_healthy())' 2

# Resume diagnostics only: expose queued type-103 tail and whether the data TCP
# object is still attached. This does not alter pacing, anchors or media state.
Replace-Exact $solo @'
    pub splice_pad_frames: u64,
    pub uses_ptp: bool,
}
'@ @'
    pub splice_pad_frames: u64,
    pub buffered_pending_bytes: usize,
    pub buffered_connected: bool,
    pub uses_ptp: bool,
}
'@ 1
Replace-Exact $solo @'
            splice_pad_frames: self.runtime.splice_pad_frames,
            uses_ptp: self.runtime.use_ptp,
'@ @'
            splice_pad_frames: self.runtime.splice_pad_frames,
            buffered_pending_bytes: self.runtime.pending.remaining().len(),
            buffered_connected: self.ready.media.io.buffered_connected(),
            uses_ptp: self.runtime.use_ptp,
'@ 1

# Arm one observation-only +500ms probe after an inferred Buffered resume.
Replace-Exact $worker @'
                    let mut last_capture_frame_at = Instant::now();
                    let mut buffered_capture_paused = false;
'@ @'
                    let mut last_capture_frame_at = Instant::now();
                    let mut buffered_capture_paused = false;
                    let mut buffered_resume_probe: Option<(Instant, u64)> = None;
'@ 1
Replace-Exact $worker @'
                            last_capture_frame_at = Instant::now();
                            buffered_capture_paused = false;
'@ @'
                            last_capture_frame_at = Instant::now();
                            buffered_capture_paused = false;
                            buffered_resume_probe = None;
'@ 1
Replace-Exact $worker @'
                        if non_silent_edge {
                            non_silent_seen = non_silent_generation;
                        }

                        // A Buffered session parked by *capture inactivity* is
'@ @'
                        if non_silent_edge {
                            non_silent_seen = non_silent_generation;
                        }

                        if let Some((due, baseline_audio_sent)) = buffered_resume_probe {
                            if Instant::now() >= due {
                                let resume_diag = engine_thread
                                    .lock()
                                    .ok()
                                    .map(|guard| guard.diagnostics());
                                let ring_pending_bytes = ring_thread
                                    .lock()
                                    .map(|ring| ring.pending_bytes())
                                    .unwrap_or(0);
                                if let Ok(mut events) = events_thread.lock() {
                                    match resume_diag {
                                        Some(diag) => events.push(format!(
                                            "MSA INPUT Buffered resume +500ms: audio_sent_delta={}; diag={diag:?}; pcm_ring_bytes={ring_pending_bytes}.",
                                            diag.audio_sent.saturating_sub(baseline_audio_sent)
                                        )),
                                        None => events.push(format!(
                                            "MSA INPUT Buffered resume +500ms: engine diagnostics unavailable; pcm_ring_bytes={ring_pending_bytes}."
                                        )),
                                    }
                                }
                                buffered_resume_probe = None;
                            }
                        }

                        // A Buffered session parked by *capture inactivity* is
'@ 1

# At the exact rate-1 transition, snapshot sender/timeline/TCP state and use its
# audio_sent counter as the +500ms delta baseline. No behavior is changed.
Replace-Exact $worker @'
                                    let ring_has_packet = ring_thread
                                        .lock()
                                        .map(|ring| ring.has_packet())
                                        .unwrap_or(false);

                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "MSA INPUT Buffered source resumed: non-SILENT WASAPI returned; rate-1 anchor restored without FLUSHBUFFERED; MRP Playing publish={mrp_result:?}; control_healthy={control_healthy}; capture_gen={capture_generation}; non_silent_gen={non_silent_generation}; ring_has_packet={ring_has_packet}."
                                        ));
                                    }
'@ @'
                                    let ring_has_packet = ring_thread
                                        .lock()
                                        .map(|ring| ring.has_packet())
                                        .unwrap_or(false);
                                    let pcm_ring_bytes = ring_thread
                                        .lock()
                                        .map(|ring| ring.pending_bytes())
                                        .unwrap_or(0);
                                    let resume_diag = engine_thread
                                        .lock()
                                        .ok()
                                        .map(|guard| guard.diagnostics());
                                    if let Some(diag) = resume_diag {
                                        buffered_resume_probe = Some((
                                            Instant::now() + Duration::from_millis(500),
                                            diag.audio_sent,
                                        ));
                                    }

                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "MSA INPUT Buffered source resumed: non-SILENT WASAPI returned; rate-1 anchor restored without FLUSHBUFFERED; MRP Playing publish={mrp_result:?}; control_healthy={control_healthy}; capture_gen={capture_generation}; non_silent_gen={non_silent_generation}; ring_has_packet={ring_has_packet}; diag={resume_diag:?}; pcm_ring_bytes={pcm_ring_bytes}."
                                        ));
                                    }
'@ 1

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

Write-Host "Validated branch fixes applied."
git diff -- $worker $solo $gui
