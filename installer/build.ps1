# Builds Fragtape and its installer: dist\Fragtape-Setup-<version>.exe
# Needs Rust (~\.cargo\bin) and Inno Setup 6 (winget install JRSoftware.InnoSetup).
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (-not (Test-Path $cargo)) { $cargo = "cargo" }
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found: winget install JRSoftware.InnoSetup" }

# the version comes from Cargo.toml
$version = (Select-String -Path "$root\fragtape\Cargo.toml" -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value

Push-Location "$root\fragtape"
try { & $cargo build --release; if ($LASTEXITCODE) { throw "cargo build failed" } } finally { Pop-Location }

& $iscc "/DAppVersion=$version" "$PSScriptRoot\fragtape.iss"
if ($LASTEXITCODE) { throw "Inno Setup failed" }
Write-Host "Built $root\dist\Fragtape-Setup-$version.exe"
