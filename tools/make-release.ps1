#Requires -Version 7
<#
.SYNOPSIS
    Build the release package the self-updater consumes.

.DESCRIPTION
    Builds the app and the TAP DLL, stages exactly the files an install
    directory needs (winbeautify.exe + beautify_taskbar_tap.dll), zips them,
    and writes the `.sha256` sidecar the updater verifies the download
    against.

    Asset naming is a contract with crates/beautify-update/src/release.rs:
    `WinBeautify-v<version>-x64.zip` + `.sha256`, uploaded to a GitHub release
    tagged `v<version>`.

.EXAMPLE
    pwsh tools/make-release.ps1            # → target/release/dist/
#>
param(
    # Where the zip and its sidecar land (relative to the repo root).
    [string]$OutDir = "target/release/dist"
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

# The workspace version from [workspace.package] — the same value the exe
# reports, which is what the updater compares the release feed against.
$version = (Select-String -Path Cargo.toml -Pattern '^\s*version\s*=\s*"(\d+\.\d+\.\d+)"').Matches[0].Groups[1].Value
$tag = "v$version"
$zipName = "WinBeautify-$tag-x64.zip"

Write-Host "== building winbeautify $tag =="
cargo build --release -p winbeautify -p beautify-taskbar-tap
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

# Stage exactly the payload layout apply_payload expects: flat, exe at the
# root, nothing else — the install directory must not inherit build noise.
$stage = Join-Path ([IO.Path]::GetTempPath()) "winbeautify-release-$version"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage | Out-Null
Copy-Item "target/release/winbeautify.exe" $stage
Copy-Item "target/release/beautify_taskbar_tap.dll" $stage

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$zip = Join-Path $OutDir $zipName
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force

$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -Path "$zip.sha256" -Value "$hash  $zipName"

Remove-Item $stage -Recurse -Force

Write-Host ""
Write-Host "发布包就绪:"
Write-Host "  $zip"
Write-Host "  $zip.sha256  ($hash)"
Write-Host "把这两个文件传到 GitHub Release(tag $tag)的附件里,更新器即可发现它。"
