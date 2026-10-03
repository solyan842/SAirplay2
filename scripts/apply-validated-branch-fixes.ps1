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

# GUI top control cards: frame inner margins are 10px per side = 20px total.
# The old 16px subtraction made every card render 4px wider than its allocated rect,
# so two adjacent cards consumed the nominal 8px gap and visually touched.
Replace-Exact $gui '        const CARD_HORIZONTAL_MARGIN: f32 = 16.0;' '        const CARD_HORIZONTAL_MARGIN: f32 = 20.0;' 1

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
