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

Write-Host "Validated branch fixes applied."
git diff -- $gui
