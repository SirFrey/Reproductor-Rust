# UI work with hot reload: runs the app (debug build) with Slint's live preview.
# Edit ui/app.slint and save: the running window updates in place, with your
# real library and playback, without recompiling.
#
#   .\scripts\ui-dev.ps1              # normal
#   .\scripts\ui-dev.ps1 -Playing     # starts the first song muted
#   .\scripts\ui-dev.ps1 -SignIn      # shows the sign-in screen (keeps your session)
param([switch]$Playing, [switch]$SignIn)

$env:SLINT_LIVE_PREVIEW = "1"
# Own build folder: switching features in target\debug would recompile Slint
# every time you go back and forth between this and plain `cargo run`.
$env:CARGO_TARGET_DIR = Join-Path $PSScriptRoot "..\target\ui-dev"
if ($Playing) { $env:REPRODUCTOR_PREVIEW_PLAYING = "1" }
if ($SignIn) { $env:REPRODUCTOR_PREVIEW_SIGNIN = "1" }
try {
    cargo run --features live-preview
} finally {
    Remove-Item Env:SLINT_LIVE_PREVIEW, Env:CARGO_TARGET_DIR, Env:REPRODUCTOR_PREVIEW_PLAYING, Env:REPRODUCTOR_PREVIEW_SIGNIN -ErrorAction SilentlyContinue
}
