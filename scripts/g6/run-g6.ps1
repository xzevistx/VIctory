# Runs G6 (the Xbox Series X build of the open-world title) the way the g6-vt2
# branch is tested: the "Vice-Tay 4070" switch set plus the g6 settings
# below. Point -GameDir at your own dump of the title; nothing else is needed.
#
#   .\scripts\g6\run-g6.ps1 -GameDir 'D:\Games\G6'
#   .\scripts\g6\run-g6.ps1 -GameDir 'D:\Games\G6' -Exe 'target\diag\victory.exe' -Extra 'VICTORY_LAZY_ALIAS=1'
#
# -Exe defaults to the diagnostics build (cargo build --profile diag -p victory
# --features diagnostics). Logs go to <DataRoot>\events.txt. F10 toggles the
# in-game panel; F8 saves the picture shown to <DataRoot>\frames.
param(
    [Parameter(Mandatory)] [string]$GameDir,
    [string]$Exe = "$PSScriptRoot\..\..\target\diag\victory.exe",
    [string]$DataRoot = "$env:LOCALAPPDATA\victory-g6",
    [string]$Console = 'series-x',
    [string]$Display = '1920x1080',
    [string]$Extra = ''
)

Get-ChildItem Env: | Where-Object { $_.Name -like 'VICTORY_*' } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
$s = [ordered]@{
    # Vice-Tay's scripts/play.ps1 -Machine 4070 (TaynanGT, GPL-3.0), not -Safe:
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    # Not VICTORY_INDIRECT_WINDOW=1 / VICTORY_INDIRECT_BATCH=2 (g6-vt2): with them G6's
    # tiled passes read stale tile counts and left black blocks on the water and
    # blocky textures (runs fix-t1 against fix-t2, same scenes; no slower without).
    VICTORY_JOIN_COPIES = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_TITLE_PRIORITIES = '0'; VICTORY_SHOW_AFTER = '1'; VICTORY_D3D12_DRED = 'light'; VICTORY_PLACED_ORDER = '1'
    VICTORY_THIN_THREADS = '8'; VICTORY_UNDER_THREADS = '6'; VICTORY_LAND_THREADS = '4'; VICTORY_PIPELINE_WORKERS = '3'
    VICTORY_CARD_RESERVE_BUDGETS = '1'; VICTORY_CARD_BUFFERS_MIN_KB = '64'; VICTORY_CARD_BUFFERS_MB = '1536'
    VICTORY_CARD_WINDOWS_MB = '1024'; VICTORY_THIN = '1'; VICTORY_JOIN_PARTS = '8'; VICTORY_UPLOAD_AHEAD = '1'
    # g6-vt2 reference settings (see G6-FORK.md, "Switches that matter"):
    VICTORY_WAVE_NATIVE = '0'; VICTORY_LAZY_ALIAS = '0'; VICTORY_HIGH_VIEWS = '0'; VICTORY_UPLOAD_SLOTS_TOP_DOWN = '0'
    VICTORY_CARD_BUFFERS_FIRST = '0'; VICTORY_MAP_THREADS = '1'; VICTORY_PIPELINE_WAIT_MS = '500'
}
foreach ($k in $s.Keys) { Set-Item "Env:$k" $s[$k] }
foreach ($pair in ($Extra -split ';' | Where-Object { $_ })) {
    $k, $v = $pair -split '=', 2
    Set-Item "Env:$k" $v
}
New-Item -ItemType Directory -Force "$DataRoot\data", "$DataRoot\cache", "$DataRoot\frames" | Out-Null
$env:VICTORY_GAME_DIR = $GameDir
$env:VICTORY_DATA_DIR = "$DataRoot\data"
$env:VICTORY_CACHE_DIR = "$DataRoot\cache"
$env:VICTORY_CAPTURE = "$DataRoot\frames"
$env:VICTORY_DISPLAY_MODE = $Display
$env:VICTORY_LOG = 'info'
& $Exe run "$GameDir\game_scarlett_master.exe" --console $Console --execute 2> "$DataRoot\events.txt"
exit $LASTEXITCODE
