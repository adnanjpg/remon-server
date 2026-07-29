<#
.SYNOPSIS
    Installs remon-server on Windows and arranges for it to start at boot.

.DESCRIPTION
    Downloads the release build, verifies it against the published checksums,
    installs it under Program Files with its state in ProgramData, and
    registers a scheduled task that runs it as SYSTEM at startup and restarts
    it if it exits.

    Re-running upgrades in place; the configuration and database are left
    alone, so this is also the upgrade path.

    Why a scheduled task and not a Windows service: a service has to speak the
    Service Control Manager protocol from inside the process (report
    START_PENDING, RUNNING, handle stop controls). remon-server is a plain
    console program, so registering it with `sc.exe create` would start it and
    then fail with "the service did not respond in a timely fashion" (1053).
    A startup task running as SYSTEM with restart-on-failure covers what a
    service is wanted for here — starts on boot, runs privileged, comes back
    after a crash — without a protocol the binary does not implement.

.PARAMETER Version
    Release tag to install, e.g. v0.18.0. Defaults to the latest release.

.PARAMETER InstallDir
    Where the binary goes. Defaults to "$env:ProgramFiles\remon".

.PARAMETER DataDir
    Configuration and database. Defaults to "$env:ProgramData\remon".

.PARAMETER NoService
    Install the binary only; do not register the startup task.

.PARAMETER AllowUnverified
    Install even when the release publishes no SHA256SUMS to check against.
    Needed only for releases that predate the current pipeline.

.EXAMPLE
    irm https://raw.githubusercontent.com/adnanjpg/remon-server/dev/packaging/install-windows.ps1 | iex

.EXAMPLE
    .\install-windows.ps1 -Version v0.18.0
#>

[CmdletBinding()]
param(
    [string] $Version,
    [string] $InstallDir = (Join-Path $env:ProgramFiles 'remon'),
    [string] $DataDir = (Join-Path $env:ProgramData 'remon'),
    [switch] $NoService,
    [switch] $AllowUnverified
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'adnanjpg/remon-server'
$TaskName = 'remon-server'
$LogPath = Join-Path $DataDir 'remon-server.log'

function Write-Step { param([string] $Message) Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Note { param([string] $Message) Write-Host "    $Message" -ForegroundColor DarkGray }
function Fail { param([string] $Message) Write-Host "error: $Message" -ForegroundColor Red; exit 1 }

# ── preflight ─────────────────────────────────────────────────────────────

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Fail 'must run from an elevated PowerShell (Run as Administrator).'
}

if ([Environment]::Is64BitOperatingSystem -eq $false) {
    Fail '32-bit Windows is not supported; only x86_64 builds are published.'
}

# ── resolve the release ───────────────────────────────────────────────────

if (-not $Version) {
    Write-Step 'Resolving latest release'
    try {
        $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
            -Headers @{ 'User-Agent' = 'remon-installer' }
        $Version = $latest.tag_name
    } catch {
        Fail "could not determine the latest release ($($_.Exception.Message)); pass -Version vX.Y.Z"
    }
}

$archive = 'remon-server-windows-amd64.zip'
$baseUrl = "https://github.com/$Repo/releases/download/$Version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) "remon-install-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    Write-Step "Downloading remon-server $Version (windows-amd64)"
    $archivePath = Join-Path $tmp $archive
    try {
        Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile $archivePath -UseBasicParsing
    } catch {
        Fail "download failed — does $Version have a windows-amd64 build?"
    }

    # The checksum is the only thing tying these bytes to the tag that was asked
    # for, and what runs afterwards runs as SYSTEM — so being unable to check is
    # a stop, not a note. Releases that predate the current pipeline publish no
    # SHA256SUMS, hence a deliberate way through rather than no way at all.
    $sumsPath = Join-Path $tmp 'SHA256SUMS'
    $haveSums = $true
    try {
        Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing
    } catch {
        $haveSums = $false
        if (-not $AllowUnverified) {
            Fail "no SHA256SUMS published for $Version; refusing to install unverified (pass -AllowUnverified to override)"
        }
        Write-Warning "no SHA256SUMS published for $Version; installing unverified because -AllowUnverified was passed"
    }

    if ($haveSums) {
        Write-Step 'Verifying checksum'
        $expected = $null
        foreach ($line in Get-Content $sumsPath) {
            $parts = $line -split '\s+', 2
            if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $archive) {
                $expected = $parts[0].ToLowerInvariant()
                break
            }
        }
        if (-not $expected) { Fail "no checksum published for $archive" }
        $actual = (Get-FileHash -Path $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($expected -ne $actual) { Fail 'checksum mismatch — refusing to install' }
    }

    Write-Step 'Unpacking'
    Expand-Archive -Path $archivePath -DestinationPath $tmp -Force
    $newBinary = Get-ChildItem -Path $tmp -Filter 'remon-server.exe' -Recurse |
        Select-Object -First 1
    if (-not $newBinary) { Fail 'archive did not contain remon-server.exe' }

    # ── data directory ────────────────────────────────────────────────────

    # ProgramData hands every local user read access to what it holds and the
    # right to create entries in it, and whoever creates a directory there keeps
    # full control of that directory's contents through CREATOR OWNER. This one
    # ends up holding the database — JWT secret, token hashes — the log the
    # pairing code is printed to, and the wrapper the startup task executes as
    # SYSTEM. So it is given an explicit ACL before anything is written into it,
    # and a directory somebody else already owns is refused rather than adopted.
    # Well-known SIDs, not names: the builtin accounts are localised, so
    # "BUILTIN\Administrators" does not exist as such on a non-English install.
    $sidSystem = 'S-1-5-18'
    $sidAdmins = 'S-1-5-32-544'

    # Created without -Force so that "this already existed" comes from the
    # create itself. -Force succeeds either way, so a separate Test-Path had to
    # answer it — and any local user may create entries under ProgramData, so
    # a directory planted between the two was reported as new and skipped the
    # ownership check written to catch exactly that.
    $dataExisted = $false
    try {
        New-Item -ItemType Directory -Path $DataDir -ErrorAction Stop | Out-Null
    } catch {
        if (-not (Test-Path -LiteralPath $DataDir -PathType Container)) { throw }
        $dataExisted = $true
    }

    if ($dataExisted) {
        $acl = Get-Acl -Path $DataDir
        $ownerSid = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
        if ($ownerSid -ne $sidSystem -and
            $ownerSid -ne $sidAdmins -and
            $ownerSid -ne $identity.User.Value) {
            $ownerName = $acl.Owner
            Fail "$DataDir already exists and is owned by $ownerName — everything in it, config.toml included, is under that account's control. Remove it and re-run."
        }
    }

    $acl = Get-Acl -Path $DataDir
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($ace in @($acl.Access)) { $null = $acl.RemoveAccessRule($ace) }
    foreach ($who in @($sidSystem, $sidAdmins)) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($who),
            'FullControl', 'ContainerInherit, ObjectInherit', 'None', 'Allow'))
    }
    # An owner holds WRITE_DAC whatever the DACL says, so locking the ACL down
    # and leaving the directory owned by whoever created it hands that account
    # the means to give itself access straight back. Under ProgramData,
    # CREATOR OWNER makes that account an ordinary local user in precisely the
    # case this block exists for.
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new($sidAdmins))
    Set-Acl -Path $DataDir -AclObject $acl

    # The binary carries its own defaults, so this file exists to be edited,
    # not to be required. Never overwrite an operator's copy on upgrade.
    $configPath = Join-Path $DataDir 'config.toml'
    if (Test-Path $configPath) {
        # Securing the directory is not atomic with creating it, and this file
        # may predate both. config.toml is executable input — `smart.smartctl_path`
        # on its own names a binary the server then runs as SYSTEM — so it is
        # honoured only when it belongs to an account that could have put it
        # there legitimately.
        $configAcl = Get-Acl -Path $configPath
        $configOwner = $configAcl.GetOwner([Security.Principal.SecurityIdentifier]).Value
        if ($configOwner -ne $sidSystem -and
            $configOwner -ne $sidAdmins -and
            $configOwner -ne $identity.User.Value) {
            Fail "$configPath is owned by $($configAcl.Owner) — it names binaries this server runs as SYSTEM. Remove it and re-run."
        }
        Write-Note "kept existing $configPath"
    } else {
        $sample = Get-ChildItem -Path $tmp -Filter 'config.toml.sample' -Recurse |
            Select-Object -First 1
        if ($sample) {
            Copy-Item -Path $sample.FullName -Destination $configPath -Force
        } else {
            @'
# remon-server configuration. Every key is optional — defaults are compiled
# into the binary. See `remon-server --help` and CONFIG.md for the full set.

[server]
port = 8080
host = "0.0.0.0"
trusted_proxy = false

[logging]
level = "info"
format = "compact"

[cors]
# Browser clients only. Native apps authenticate with bearer tokens and are
# unaffected by anything here. Add your web UI's origin to use one:
# allowed_origins = ["https://app.example.com"]
allow_any_origin = false
allowed_origins = []
'@ | Set-Content -Path $configPath -Encoding UTF8
        }
        # Owned by Administrators rather than by whoever ran the installer, so
        # the check above still passes when the next upgrade is run from a
        # different administrator account.
        $configAcl = Get-Acl -Path $configPath
        $configAcl.SetOwner([Security.Principal.SecurityIdentifier]::new($sidAdmins))
        Set-Acl -Path $configPath -AclObject $configAcl
        Write-Note "wrote $configPath"
    }

    # Checked with the build about to be installed, and before the running task
    # is stopped. A config this build rejects then leaves the existing install
    # untouched and still serving, instead of stopped and already overwritten.
    Write-Step 'Validating configuration'
    & $newBinary.FullName --config-dir $DataDir --data-dir $DataDir config check
    if ($LASTEXITCODE -ne 0) {
        Fail 'configuration did not validate; nothing was changed'
    }

    # ── stop and install ──────────────────────────────────────────────────

    $existingTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $wasRunning = $false
    if ($existingTask -and $existingTask.State -eq 'Running') {
        $wasRunning = $true
        Write-Step "Stopping $TaskName for upgrade"
        Stop-ScheduledTask -TaskName $TaskName
        # The task hosts a wrapper that owns the server as a child, so wait for
        # the binary itself rather than the task's own state.
        $deadline = (Get-Date).AddSeconds(30)
        while ((Get-Date) -lt $deadline -and
               (Get-Process -Name 'remon-server' -ErrorAction SilentlyContinue)) {
            Start-Sleep -Milliseconds 500
        }
        Get-Process -Name 'remon-server' -ErrorAction SilentlyContinue |
            Stop-Process -Force -ErrorAction SilentlyContinue
    }

    $binary = Join-Path $InstallDir 'remon-server.exe'
    Write-Step "Installing to $binary"
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -Path $newBinary.FullName -Destination $binary -Force

    # ── startup task ──────────────────────────────────────────────────────

    if ($NoService) {
        # Skipping task *setup* is not a request to leave the host unmonitored.
        # Whatever was running when this started gets put back on the new binary.
        if ($wasRunning) {
            Write-Step "Restarting $TaskName"
            Start-ScheduledTask -TaskName $TaskName
        }
        Write-Host ''
        Write-Host 'Installed.' -ForegroundColor Green -NoNewline
        Write-Host ' Startup task skipped (-NoService).'
        Write-Host "Run it with: `"$binary`" --config-dir `"$DataDir`" --data-dir `"$DataDir`""
        exit 0
    }

    # A scheduled task cannot redirect output, and the pairing code and the
    # getting-started block are written to stdout — so run through a wrapper
    # that captures them where an operator can read them back.
    $wrapperPath = Join-Path $DataDir 'run-remon-server.cmd'
    @"
@echo off
rem Generated by install-windows.ps1. Edited by hand, it will be overwritten
rem on the next upgrade.
set "LOG=$LogPath"
rem Keep the log from growing without bound across restarts.
for %%A in ("%LOG%") do if exist "%LOG%" if %%~zA GTR 10485760 move /y "%LOG%" "%LOG%.1" >nul
"$binary" --config-dir "$DataDir" --data-dir "$DataDir" >> "%LOG%" 2>&1
"@ | Set-Content -Path $wrapperPath -Encoding ASCII

    Write-Step 'Registering the startup task'
    $action = New-ScheduledTaskAction -Execute $wrapperPath
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' `
        -LogonType ServiceAccount -RunLevel Highest
    # ExecutionTimeLimit zero means "no limit" — without it the task is killed
    # after three days. The restart count is the ceiling Task Scheduler allows,
    # so the agent keeps coming back rather than giving up after a handful of
    # attempts; the counter resets at each boot.
    #
    # Task Scheduler only restarts an action that *failed*, so this covers a
    # crash and covers being killed (the kill path exits 1). It does not cover
    # a deliberate clean exit — which is why the server exits non-zero when it
    # restarts itself, so every supervisor treats that as restart-worthy.
    $settings = New-ScheduledTaskSettingsSet `
        -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
        -MultipleInstances IgnoreNew `
        -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
        -ExecutionTimeLimit ([TimeSpan]::Zero)

    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings `
        -Description 'Remon monitoring server' -Force | Out-Null

    Write-Step "Starting $TaskName"
    Start-ScheduledTask -TaskName $TaskName

    # ── verify ────────────────────────────────────────────────────────────

    $port = 8080
    $portMatch = Select-String -Path $configPath -Pattern '^\s*port\s*=\s*(\d+)' |
        Select-Object -First 1
    if ($portMatch) { $port = [int] $portMatch.Matches[0].Groups[1].Value }

    $healthy = $false
    for ($i = 0; $i -lt 30; $i++) {
        try {
            Invoke-WebRequest -Uri "http://127.0.0.1:$port/health" -TimeoutSec 2 `
                -UseBasicParsing | Out-Null
            $healthy = $true
            break
        } catch {
            Start-Sleep -Seconds 1
        }
    }

    $address = (Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object { $_.IPAddress -ne '127.0.0.1' -and $_.PrefixOrigin -ne 'WellKnown' } |
        Select-Object -First 1 -ExpandProperty IPAddress)
    if (-not $address) { $address = '127.0.0.1' }

    Write-Host ''
    if ($healthy) {
        Write-Host "remon-server $Version is running." -ForegroundColor Green
        Write-Host ''
        Write-Host "  http://${address}:$port" -ForegroundColor White
        Write-Host ''
        if ($wasRunning) {
            Write-Note 'upgraded in place; configuration and database untouched'
        } else {
            Write-Note 'add the address above in the Remon app, then start pairing'
            Write-Note "the 8-digit code appears in: $LogPath"
        }
        Write-Host ''
        Write-Note "config   $configPath"
        Write-Note "data     $DataDir"
        Write-Note "diagnose `"$binary`" --config-dir `"$DataDir`" --data-dir `"$DataDir`" doctor"
    } else {
        Write-Warning "the server did not answer on http://127.0.0.1:$port/health"
        Write-Host ''
        Write-Host "  Get-ScheduledTaskInfo -TaskName $TaskName"
        Write-Host "  Get-Content `"$LogPath`" -Tail 50"
        Write-Host "  & `"$binary`" --config-dir `"$DataDir`" --data-dir `"$DataDir`" doctor"
        exit 1
    }
} finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
