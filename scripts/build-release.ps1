# Builds the portable release snippy.exe.
#
# Rust records source paths in the binary (panic and error locations). They would include the
# builder's user profile and checkout folder, so they are rewritten to neutral prefixes here:
# the user profile (Cargo registry, rustup toolchains) becomes "~" and the checkout "snippy".

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

$prefixes = [ordered]@{}
$prefixes[$env:USERPROFILE] = '~'
if ($env:CARGO_HOME) { $prefixes[$env:CARGO_HOME] = 'cargo' }
if ($env:RUSTUP_HOME) { $prefixes[$env:RUSTUP_HOME] = 'rustup' }
# Listed last so it wins when the checkout sits inside the user profile.
$prefixes[$root] = 'snippy'

$flags = foreach ($entry in $prefixes.GetEnumerator()) {
    if ($entry.Key) { "--remap-path-prefix=$($entry.Key)=$($entry.Value)" }
}
# The encoded form keeps paths with spaces intact (arguments are separated by 0x1F).
$env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1F

Push-Location $root
try {
    npx tauri build
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}
