# Builds Acme Memo, the example plugin of docs/plugin-authors.md, and packs it as SpelaionDB adds
# it: a .tar.gz with spelaion-plugin.toml and the program at its top. Windows only: it uses the
# C# compiler of the .NET Framework every Windows has, and nothing to download.
#
#   pwsh examples/plugins/acme-memo/build.ps1 [-Out <folder>]
#
# Then, in SpelaionDB: Settings > Plugins > Plugins you added > Add from a file...

param([string]$Out = (Join-Path $PSScriptRoot "dist"))

$ErrorActionPreference = "Stop"
$csc = Join-Path $env:WINDIR "Microsoft.NET\Framework64\v4.0.30319\csc.exe"
if (-not (Test-Path $csc)) { throw "The .NET Framework's C# compiler is not at $csc" }

$work = Join-Path $Out "work"
New-Item -ItemType Directory -Force $work | Out-Null
$program = "acme-memo-plugin.exe"
& $csc /nologo /optimize+ /target:exe /r:System.Web.Extensions.dll "/out:$(Join-Path $work $program)" (Join-Path $PSScriptRoot "Plugin.cs")
if ($LASTEXITCODE -ne 0) { throw "csc failed" }

$manifest = @"
schema_version = 1
name = "acme-memo"
version = "1.0.0"
author = "Acme Ltd. (an example in SpelaionDB's repository)"
description = "Notes kept in memory, to show how a plugin is written."
protocol = "1.0"
platform = "x86_64-pc-windows-msvc"
program = "$program"
engine = "acme-memo"
engine_label = "Acme Memo"
"@
# UTF-8 without a byte-order mark, with LF line ends.
[System.IO.File]::WriteAllText((Join-Path $work "spelaion-plugin.toml"), $manifest.Replace("`r`n", "`n") + "`n")

$archive = Join-Path $Out "acme-memo-1.0.0-x86_64-pc-windows-msvc.tar.gz"
# Windows' own tar: the two files at the archive's top, by their own names.
& tar.exe -czf $archive -C $work spelaion-plugin.toml $program
if ($LASTEXITCODE -ne 0) { throw "tar failed" }
$hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
$size = (Get-Item $archive).Length
Write-Output "$archive"
Write-Output "archive_sha256 = `"$hash`""
Write-Output "archive_size = $size"
