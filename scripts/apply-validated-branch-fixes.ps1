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
$gui = "crates/sairplay-gui/src/main.rs"

# Windows loopback has no explicit PAUSE/PLAY lifecycle. Do not map a capture
# gap to MSA's rate-0/rate-1 un-pause path: live Naim testing proved that the
# sender continues cleanly after rate-1 while the receiver can stay silent.
# Instead, treat a sustained no-frame gap as a parked session boundary:
# MSA STANDBY performs rate-0 + FLUSHBUFFERED + CONNECTED, local PCM is then
# flushed, and only fresh post-flush non-silent PCM re-arms deferred START.
Replace-Exact $worker @'
                    let mut last_capture_frame_at = Instant::now();
                    let mut buffered_capture_paused = false;
'@ @'
                    let mut last_capture_frame_at = Instant::now();
                    let mut buffered_capture_parked = false;
                    let mut buffered_park_flush_target: Option<u64> = None;
'@ 1

Replace-Exact $worker @'
                            non_silent_seen =
                                pcm_hub_thread.non_silent_generation();
                            last_capture_frame_at = Instant::now();
                            buffered_capture_paused = false;
'@ @'
                            non_silent_seen =
                                pcm_hub_thread.non_silent_generation();
                            last_capture_frame_at = Instant::now();
                            if buffered_park_flush_target
                                .map(|target| ack >= target)
                                .unwrap_or(false)
                            {
                                buffered_park_flush_target = None;
                            }
'@ 1

Replace-Exact $worker @'
                        // A Buffered session parked by *capture inactivity* is
                        // resumed only by fresh non-silent source data. The
                        // producer has already placed those first samples in
                        // the bounded ring, so play_content() establishes the
                        // new rate-1 anchor before the consumer releases them.
                        if buffered_capture_paused && non_silent_edge {
                            // Match MSA/cliairplay ordering: change the audio
                            // state under the engine lock, then publish MRP only
                            // after that lock has been released. Third-party
                            // buffered receivers can use this state transition
                            // to follow the fresh rate-1 anchor reliably.
                            let (resume_result, resume_mrp) = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                if guard.is_buffered()
                                    && guard.content_paused()
                                    && guard.runtime.state == Ap2State::Paused
                                {
                                    let result = guard.play_content().map(|_| true);
                                    let mrp = if result.is_ok() {
                                        guard.mrp_controller()
                                    } else {
                                        None
                                    };
                                    (result, mrp)
                                } else {
                                    (Ok(false), None)
                                }
                            };
                            match resume_result {
                                Ok(true) => {
                                    buffered_capture_paused = false;
                                    last_capture_frame_at = Instant::now();

                                    let mrp_result = resume_mrp.map(|mrp| {
                                        mrp.publish_playback_state(
                                            crate::MrpPlaybackState::Playing,
                                            true,
                                        )
                                    });
                                    let control_healthy = engine_thread
                                        .lock()
                                        .map(|guard| guard.control_healthy())
                                        .unwrap_or(false);
                                    let ring_fill = ring_thread
                                        .lock()
                                        .map(|ring| ring.fill())
                                        .unwrap_or(0);

                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "MSA INPUT Buffered source resumed: non-SILENT WASAPI returned; rate-1 anchor restored without FLUSHBUFFERED; MRP Playing publish={mrp_result:?}; control_healthy={control_healthy}; capture_gen={capture_generation}; non_silent_gen={non_silent_generation}; ring_fill={ring_fill}."
                                        ));
                                    }
                                }
                                Ok(false) => {
                                    // An explicit GUI/session lifecycle command
                                    // may have superseded the inferred park.
                                    buffered_capture_paused = false;
                                }
                                Err(e) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some(format!(
                                            "Buffered capture-idle resume failed: {e:?}"
                                        ));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }
'@ @'
                        // A capture-idle park is resumed only after the producer
                        // has acknowledged the local PCM flush and fresh
                        // non-silent PCM arrives. Re-arm the existing deferred
                        // START path; NativeSoloEngine::start() then uses MSA's
                        // post-FLUSH buffered resume and creates a fresh rate-1
                        // anchor instead of attempting an in-place un-pause.
                        if buffered_capture_parked
                            && buffered_park_flush_target.is_none()
                            && non_silent_edge
                        {
                            let restart_allowed = {
                                let guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                guard.is_buffered()
                                    && guard.runtime.state == Ap2State::Connected
                                    && guard.content_stopped()
                            };

                            if restart_allowed {
                                buffered_capture_parked = false;
                                deferred_audio_seen = None;
                                deferred_start_thread.store(true, Ordering::SeqCst);
                                last_capture_frame_at = Instant::now();
                                if let Ok(mut events) = events_thread.lock() {
                                    events.push(
                                        "MSA INPUT Buffered source resumed after idle park: fresh post-flush PCM detected; deferred Buffered START re-armed."
                                            .into(),
                                    );
                                }
                            } else {
                                // Another explicit lifecycle command superseded
                                // the inferred park. Do not manufacture a START.
                                buffered_capture_parked = false;
                            }
                        }
'@ 1

Replace-Exact $worker @'
                        // MSA receives PAUSE/PLAY as explicit session commands.
                        // Windows loopback has no equivalent EOF/pause event, so
                        // infer PAUSE only when the endpoint returns *no frames
                        // at all* for a sustained interval. SILENT packets count
                        // as live PCM and continuously reset this timer.
                        if !buffered_capture_paused
                            && pcm_hub_thread.source_present()
                        {
                            // Keep MRP publication outside the engine/audio
                            // critical section, matching upstream cliairplay's
                            // PAUSE transition ordering.
                            let (park_result, park_mrp) = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                if buffered_capture_idle_should_park(
                                    true,
                                    last_capture_frame_at.elapsed(),
                                    guard.is_buffered(),
                                    guard.runtime.state,
                                    guard.content_paused(),
                                    guard.content_stopped(),
                                ) {
                                    let result = guard.pause_content().map(|_| true);
                                    let mrp = if result.is_ok() {
                                        guard.mrp_controller()
                                    } else {
                                        None
                                    };
                                    (result, mrp)
                                } else {
                                    (Ok(false), None)
                                }
                            };
                            match park_result {
                                Ok(true) => {
                                    buffered_capture_paused = true;
                                    // Keep the last observed non-silent generation.
                                    // If source audio returns concurrently with
                                    // the rate-0 park, the next media-loop turn
                                    // must still observe that edge and resume.
                                    let mrp_result = park_mrp.map(|mrp| {
                                        mrp.publish_playback_state(
                                            crate::MrpPlaybackState::Paused,
                                            true,
                                        )
                                    });
                                    let control_healthy = engine_thread
                                        .lock()
                                        .map(|guard| guard.control_healthy())
                                        .unwrap_or(false);
                                    let ring_fill = ring_thread
                                        .lock()
                                        .map(|ring| ring.fill())
                                        .unwrap_or(0);

                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "MSA INPUT Buffered capture idle for >={}ms: rate-0 PAUSE armed; receiver buffer preserved, no FLUSHBUFFERED; MRP Paused publish={mrp_result:?}; control_healthy={control_healthy}; capture_gen={capture_generation}; ring_fill={ring_fill}.",
                                            BUFFERED_CAPTURE_IDLE_PARK_INTERVAL.as_millis()
                                        ));
                                    }
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some(format!(
                                            "Buffered capture-idle pause failed: {e:?}"
                                        ));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }
'@ @'
                        // MSA receives PAUSE/PLAY as explicit commands; a
                        // Windows capture gap is not one. For Buffered type103,
                        // park the session with MSA STANDBY only after sustained
                        // absence of *all* capture frames. STANDBY performs the
                        // protocol-native rate-0 + FLUSHBUFFERED boundary and
                        // leaves the live session CONNECTED for a clean restart.
                        if !buffered_capture_parked
                            && pcm_hub_thread.source_present()
                        {
                            let (park_result, park_mrp) = {
                                let mut guard = match engine_thread.lock() {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Ok(mut slot) = error_thread.lock() {
                                            *slot = Some("native SOLO engine mutex poisoned".into());
                                        }
                                        running_thread.store(false, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                if buffered_capture_idle_should_park(
                                    true,
                                    last_capture_frame_at.elapsed(),
                                    guard.is_buffered(),
                                    guard.runtime.state,
                                    guard.content_paused(),
                                    guard.content_stopped(),
                                ) {
                                    let result = guard.standby().map(|_| true);
                                    let mrp = if result.is_ok() {
                                        guard.mrp_controller()
                                    } else {
                                        None
                                    };
                                    (result, mrp)
                                } else {
                                    (Ok(false), None)
                                }
                            };

                            match park_result {
                                Ok(true) => {
                                    buffered_capture_parked = true;
                                    deferred_start_thread.store(false, Ordering::SeqCst);
                                    deferred_audio_seen = None;

                                    // STANDBY has already flushed the receiver.
                                    // Now discard every pre-boundary local PCM
                                    // byte before allowing the fresh START.
                                    let flush_target = pcm_hub_thread.request_flush();
                                    buffered_park_flush_target = Some(flush_target);

                                    let mrp_result = park_mrp.map(|mrp| {
                                        mrp.publish_playback_state(
                                            crate::MrpPlaybackState::Paused,
                                            true,
                                        )
                                    });
                                    if let Ok(mut events) = events_thread.lock() {
                                        events.push(format!(
                                            "MSA INPUT Buffered capture idle for >={}ms: STANDBY + FLUSHBUFFERED completed; local PCM flush generation={flush_target}; waiting for fresh post-flush PCM before deferred START; MRP Paused publish={mrp_result:?}.",
                                            BUFFERED_CAPTURE_IDLE_PARK_INTERVAL.as_millis()
                                        ));
                                    }
                                }
                                Ok(false) => {}
                                Err(e) => {
                                    if let Ok(mut slot) = error_thread.lock() {
                                        *slot = Some(format!(
                                            "Buffered capture-idle standby failed: {e:?}"
                                        ));
                                    }
                                    running_thread.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
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
git diff -- $worker $gui
