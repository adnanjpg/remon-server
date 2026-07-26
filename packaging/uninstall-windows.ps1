<#
.SYNOPSIS
    Removes remon-server from Windows.

.DESCRIPTION
    Stops and unregisters the startup task and deletes the binary. The
    configuration and database are left alone unless -Purge is given, because
    "uninstall" and "throw away my history" are different intentions and only
    one of them is recoverable.

.PARAMETER Purge
    Also delete the configuration, database and logs.
#>

[CmdletBinding()]
param(
    [string] $InstallDir = (Join-Path $env:ProgramFiles 'remon'),
    [string] $DataDir = (Join-Path $env:ProgramData 'remon'),
    [switch] $Purge
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$TaskName = 'remon-server'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'error: must run from an elevated PowerShell (Run as Administrator).' -ForegroundColor Red
    exit 1
}

$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($task) {
    if ($task.State -eq 'Running') {
        Write-Host "==> Stopping $TaskName"
        Stop-ScheduledTask -TaskName $TaskName
    }
    Write-Host "==> Unregistering $TaskName"
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
}

# The task hosts a wrapper that owns the server as a child, so stopping the
# task does not reliably take the binary with it.
Get-Process -Name 'remon-server' -ErrorAction SilentlyContinue |
    Stop-Process -Force -ErrorAction SilentlyContinue

$binary = Join-Path $InstallDir 'remon-server.exe'
if (Test-Path $binary) {
    Write-Host "==> Removing $binary"
    Remove-Item -Path $binary -Force
    # Only if we left it empty — an operator may keep other things there.
    if (-not (Get-ChildItem -Path $InstallDir -Force -ErrorAction SilentlyContinue)) {
        Remove-Item -Path $InstallDir -Force
    }
}

$wrapper = Join-Path $DataDir 'run-remon-server.cmd'
if (Test-Path $wrapper) { Remove-Item -Path $wrapper -Force }

Write-Host ''
if ($Purge) {
    if (Test-Path $DataDir) {
        Write-Host "==> Removing $DataDir"
        Remove-Item -Path $DataDir -Recurse -Force
    }
    Write-Host 'remon-server removed, including configuration and metrics history.'
} else {
    Write-Host 'remon-server removed.'
    if (Test-Path $DataDir) {
        Write-Host "Kept $DataDir (configuration, metrics history, pairings)"
    }
    Write-Host 'Run with -Purge to delete those too.'
}
