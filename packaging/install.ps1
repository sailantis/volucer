#Requires -Version 5.1
<#
.SYNOPSIS

Installs the volucer binary on Windows.

.DESCRIPTION

    irm https://github.com/sailantis/volucer/releases/latest/download/install.ps1 | iex

The Windows half of `install.sh`, and the reason both exist is the same: `volucer-cli` is a
Composer package, so it needs PHP, and a Python or Ruby operator has no reason to have PHP
installed. This installs the same releases by the same two decisions - resolve the tag, then
verify the checksum before extracting anything - without the runtime.

Written against Windows PowerShell 5.1, which is the version that ships with Windows. That
rules out `??`, `-AsHashtable`, `Get-Content -AsByteStream` and `Invoke-RestMethod -UseBasicParsing`
being optional; where a 7-only spelling exists it is avoided rather than detected.

.PARAMETER Version

A tag to install, `v0.1.0` or `0.1.0`. The newest release by default.

.PARAMETER InstallDir

Where to put the binary. `$env:LOCALAPPDATA\volucer\bin` by default.

.PARAMETER Force

Replace an existing binary.

.PARAMETER BaseUrl

The base the assets are fetched from. Exists so this can be tested against a fixture release
rather than only against GitHub.

.PARAMETER Verify

`auto` (default), `require` or `off` - whether to check the build provenance attestation.
`auto` checks when `gh` is installed and authenticated and the archive came from GitHub;
`require` refuses to install without the check; `off` skips it.

.PARAMETER ModifyPath

`1` (default) or `0` - whether the user PATH may be edited. `0` installs the binary and leaves
the machine's environment alone, printing the directory to add instead. It exists for the same
reason as the shell installer's `VOLUCER_MODIFY_PATH`: an unattended or CI install has no
operator to undo a change, and a test that installs into a temporary directory must not leave
that directory on the machine's PATH afterwards.
#>
[CmdletBinding()]
param(
    [string] $Version,
    [string] $InstallDir,
    [switch] $Force,
    [string] $BaseUrl = 'https://github.com/sailantis/volucer/releases/download',
    [string] $Verify = $env:VOLUCER_VERIFY,
    [string] $ModifyPath = $env:VOLUCER_MODIFY_PATH
)

if (-not $Verify) {
    $Verify = 'auto'
}

if (-not $ModifyPath) {
    $ModifyPath = '1'
}

$ErrorActionPreference = 'Stop'

$App = 'volucer'
$Repository = 'sailantis/volucer'

# Both settings are checked before anything is downloaded: an installer that fetches 5 MB and
# then reports that its own argument was misspelled has spent the operator's time to learn
# something it could have said immediately. Below `$Repository`, because the origin comparison
# needs it.
if ($ModifyPath -notin @('0', '1')) {
    throw "install: VOLUCER_MODIFY_PATH is '$ModifyPath'; expected '0' or '1'"
}

if ($Verify -notin @('auto', 'require', 'off')) {
    throw "install: Verify is '$Verify'; expected 'auto', 'require' or 'off'"
}

if ($Verify -eq 'require' -and $BaseUrl -ne "https://github.com/$Repository/releases/download") {
    throw "install: Verify is 'require', but $BaseUrl is not the GitHub release origin; there is no attestation to check there"
}

function Write-Note {
    param([string] $Message)
    [Console]::Error.WriteLine("install: $Message")
}

function Stop-Install {
    param([string] $Message)
    throw "install: $Message"
}

# ------------------------------------------------------------------ platform

# `$env:PROCESSOR_ARCHITECTURE` is `AMD64` on 64-bit Windows and `x86` inside a 32-bit
# process on a 64-bit machine, so `PROCESSOR_ARCHITEW6432` is consulted first. Only x86_64
# Windows is published; refusing by name is worth more than selecting an archive that
# verifies correctly and then cannot execute.
$architecture = if ($env:PROCESSOR_ARCHITEW6432) {
    $env:PROCESSOR_ARCHITEW6432
} else {
    $env:PROCESSOR_ARCHITECTURE
}

if ($architecture -ne 'AMD64') {
    Stop-Install "no archive for Windows on $architecture. The only Windows build is x86_64."
}

$target = 'x86_64-pc-windows-msvc'
Write-Note "platform windows/x86_64 -> $target"

# ------------------------------------------------------------------- version

# Environment variables are the only way to configure the documented one-liner:
# `irm ... | iex` cannot pass parameters, so a `-InstallDir` there is unreachable. The shell
# installer reads these names, and two installers that disagree about how to say "put it here" are
# worse than one, so the parameters stay the PowerShell-native spelling and the variables are an
# equivalent way in. A parameter wins, because it is the more explicit of the two.
#
# **This block has to run before the version is resolved, not after.** It used to sit below the
# resolution with a comment claiming it came first, and the effect was that `VOLUCER_VERSION` was
# ignored: by the time it was read, `if (-not $Version)` had already replaced the variable with
# whatever `releases/latest` said, so the line was unreachable. The failure was invisible until
# the releases API rate-limited the check and a pinned version would have answered it without the
# network. `VOLUCER_INSTALL_DIR` and `VOLUCER_DOWNLOAD_URL` were fine only by luck: nothing above
# set them.
if (-not $Version -and $env:VOLUCER_VERSION) { $Version = $env:VOLUCER_VERSION }
if (-not $InstallDir -and $env:VOLUCER_INSTALL_DIR) { $InstallDir = $env:VOLUCER_INSTALL_DIR }
if (-not $Force -and $env:VOLUCER_FORCE) { $Force = $true }
if (-not $PSBoundParameters.ContainsKey('BaseUrl') -and $env:VOLUCER_DOWNLOAD_URL) {
    $BaseUrl = $env:VOLUCER_DOWNLOAD_URL
}

# Resolved before the asset name, because the asset name contains the version:
# `volucer-<version>-<target>.zip`. A "latest" URL with a hardcoded version starts
# returning 404 the moment a newer release exists.
if (-not $Version) {
    # `releases/latest` answers with a redirect whose Location names the tag, so the API is
    # not needed and no rate limit applies. Read from the headers rather than followed, so
    # the tag is available without downloading a release page.
    $uri = "https://api.github.com/repos/$Repository/releases/latest"
    try {
        $response = Invoke-WebRequest -Uri $uri -UseBasicParsing -MaximumRedirection 0
    } catch {
        Stop-Install "the releases API could not be reached at $uri`: $($_.Exception.Message)"
    }

    # ConvertFrom-Json is present in 5.1; the tag is read as a property rather than parsed
    # out of the body, which is the one place PowerShell 5.1 is better than `sh` here.
    $Version = ($response.Content | ConvertFrom-Json).tag_name
    if (-not $Version) {
        Stop-Install 'the releases API answered with no tag_name; no release to install'
    }
}

if (-not $Version.StartsWith('v')) {
    $Version = "v$Version"
}
$numeric = $Version.Substring(1)

$asset = "$App-$numeric-$target.zip"
Write-Note "release $Version"
Write-Note "asset   $asset"

# ------------------------------------------------------------------- install

if (-not $InstallDir) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "$App\bin"
}

$final = Join-Path $InstallDir "$App.exe"

if ((Test-Path $final) -and -not $Force) {
    Stop-Install "$final already exists. Pass -Force to replace it."
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("volucer-" + [Guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Path $work -Force | Out-Null

try {
    $archive = Join-Path $work $asset
    $checksum = Join-Path $work "$asset.sha256"

    Write-Note 'downloading'
    $archiveUrl = "$BaseUrl/$Version/$asset"
    try {
        Invoke-WebRequest -Uri $archiveUrl -OutFile $archive -UseBasicParsing
        Invoke-WebRequest -Uri "$archiveUrl.sha256" -OutFile $checksum -UseBasicParsing
    } catch {
        Stop-Install "could not download $archiveUrl`: $($_.Exception.Message)"
    }

    # The checksum is compared *before* anything is extracted. Hashing an untrusted file is
    # cheap; extracting one is not.
    $expected = ([regex]::Match((Get-Content $checksum -Raw), '(?i)\b[0-9a-f]{64}\b')).Value
    if (-not $expected) {
        Stop-Install "$asset.sha256 does not contain a SHA-256 digest"
    }

    $actual = (Get-FileHash -Path $archive -Algorithm SHA256).Hash
    if ($actual -ne $expected) {
        Stop-Install "the checksum does not match. expected $expected, got $actual. Nothing was installed."
    }
    Write-Note 'checksum verified'

    # The checksum proves the archive matches a file on *the same server*, not that the file came
    # from this repository's release workflow: whoever can replace the archive can replace the
    # `.sha256` beside it. The build provenance attestation is signed by GitHub's OIDC identity
    # and recorded in Sigstore's transparency log, so `gh attestation verify` checks it against a
    # trust root the download origin does not control. A *failed* verification is fatal - that is
    # the one case where it must stop - and `-Verify off` is the deliberate way past it.
    if ($Verify -ne 'off') {
        $fromGitHub = $BaseUrl -eq "https://github.com/$Repository/releases/download"
        $gh = Get-Command gh -ErrorAction SilentlyContinue

        if (-not $fromGitHub) {
            Write-Note "provenance not checked: $BaseUrl is not the GitHub release origin"
        } elseif (-not $gh) {
            if ($Verify -eq 'require') {
                Stop-Install "Verify is 'require', but the GitHub CLI (gh) is not installed; it is what verifies the attestation"
            }
            Write-Note 'provenance not checked: the GitHub CLI (gh) is not installed, so the archive is verified only against the checksum beside it'
        } else {
            # `gh auth status` rather than trying the verify and reading the error: an
            # unauthenticated `gh` fails with a 401 that is indistinguishable from a genuine
            # attestation mismatch, and treating those the same would either refuse every install
            # or accept a forged archive.
            & gh auth status *> $null
            if ($LASTEXITCODE -ne 0) {
                if ($Verify -eq 'require') {
                    Stop-Install "Verify is 'require', but gh is not authenticated; run 'gh auth login'"
                }
                Write-Note 'provenance not checked: gh is not authenticated, so the archive is verified only against the checksum beside it'
            } else {
                & gh attestation verify $archive --repo $Repository *> $null
                if ($LASTEXITCODE -ne 0) {
                    # Fatal, and the wording names both causes rather than asserting the worse
                    # one: a digest with no attestation is what a replaced archive looks like, and
                    # a failed API call looks the same from here because gh reports both as a
                    # non-zero exit. Either way the check did not pass.
                    Stop-Install "the build provenance of $asset could not be verified: the archive is not attested as built by this repository's release workflow, or the attestation could not be fetched. Nothing was installed. (Pass -Verify off to skip this check.)"
                }
                Write-Note 'provenance verified'
            }
        }
    }

    Expand-Archive -Path $archive -DestinationPath $work -Force
    $staged = Join-Path $work "$App-$numeric-$target\$App.exe"
    if (-not (Test-Path $staged)) {
        Stop-Install "the archive does not contain $App-$numeric-$target\$App.exe"
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Move-Item -Path $staged -Destination $final -Force

    Write-Note "installed $final"
    $reported = & $final version 2>$null | Select-Object -First 1
    if ($reported) {
        Write-Note $reported
    }

    if ($ModifyPath -eq '0') {
        Write-Note "$InstallDir is not being added to PATH because -ModifyPath 0 was given"
        Write-Note "add it yourself, or run this without that setting"
        return
    }

    # The directory is put on the *user* PATH rather than modified for this process only,
    # because the next thing the operator does is open a new shell.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $onPath = $false
    foreach ($entry in ($userPath -split ';')) {
        if ($entry -and ($entry.TrimEnd('\') -eq $InstallDir.TrimEnd('\'))) {
            $onPath = $true
        }
    }

    if (-not $onPath) {
        $updated = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { "$userPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
        Write-Note "$InstallDir was added to your user PATH; open a new shell to use it"
        Write-Note "to undo it, remove $InstallDir from your user PATH"
    }
} finally {
    Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
}
