<#
.SYNOPSIS
Builds and installs Lucent for the current user, or removes it.

.DESCRIPTION
Install: builds the release exe, stops a running Lucent, puts the Windows
taskbar back (lucent.exe --restore-taskbar), copies the exe to
%LOCALAPPDATA%\Programs\Lucent, creates a Start Menu shortcut, sets the HKCU
Run value "Lucent" for autostart, and starts Lucent.

Uninstall: stops Lucent, restores the taskbar, removes the Run value, the
shortcut and the install folder.

.PARAMETER Uninstall
Remove Lucent instead of installing it.
#>
[CmdletBinding()]
param(
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repo 'src-tauri\Cargo.toml'
$built = Join-Path $repo 'src-tauri\target\release\lucent.exe'
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Lucent'
$installed = Join-Path $installDir 'lucent.exe'
$startMenuDir = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$shortcut = Join-Path $startMenuDir 'Lucent.lnk'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

function Stop-Lucent {
    $running = Get-Process -Name 'lucent' -ErrorAction SilentlyContinue
    if ($running) {
        Write-Host 'Stopping Lucent...'
        $running | Stop-Process -Force
        $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }
}

# lucent.exe is a GUI-subsystem exe, so run it with Start-Process -Wait to know
# when the restore has finished, and fail if it did not succeed.
function Restore-Taskbar([string]$exe) {
    if (-not $exe) {
        Write-Warning 'No lucent.exe found to run --restore-taskbar. If the taskbar is hidden, run lucent.exe --restore-taskbar from a build.'
        return
    }
    Write-Host 'Putting the Windows taskbar back...'
    $p = Start-Process -FilePath $exe -ArgumentList '--restore-taskbar' -Wait -PassThru -WindowStyle Hidden
    if ($p.ExitCode -ne 0) {
        throw "lucent.exe --restore-taskbar failed with exit code $($p.ExitCode). Stopping before changes are made."
    }
}

function Get-RestoreExe {
    if (Test-Path -LiteralPath $installed) { return $installed }
    if (Test-Path -LiteralPath $built) { return $built }
    return $null
}

if ($Uninstall) {
    Stop-Lucent
    Restore-Taskbar (Get-RestoreExe)

    Write-Host 'Removing autostart, shortcut and install folder...'
    Remove-ItemProperty -Path $runKey -Name 'Lucent' -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $shortcut -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $installDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host 'Lucent is uninstalled.'
    exit 0
}

Write-Host 'Building Lucent (release)...'
& cargo build --release --manifest-path $manifest
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}
if (-not (Test-Path -LiteralPath $built)) {
    throw "build finished but $built is missing"
}

Stop-Lucent
Restore-Taskbar (Get-RestoreExe)

Write-Host "Copying lucent.exe to $installDir..."
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Copy-Item -LiteralPath $built -Destination $installed -Force

Write-Host 'Creating the Start Menu shortcut...'
New-Item -ItemType Directory -Force -Path $startMenuDir | Out-Null
$wsh = New-Object -ComObject WScript.Shell
try {
    $link = $wsh.CreateShortcut($shortcut)
    $link.TargetPath = $installed
    $link.WorkingDirectory = $installDir
    $link.IconLocation = "$installed,0"
    $link.Description = 'Lucent'
    $link.Save()
} finally {
    [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($wsh)
}

Write-Host 'Enabling start with Windows...'
Set-ItemProperty -Path $runKey -Name 'Lucent' -Value ('"{0}"' -f $installed)

Write-Host 'Starting Lucent...'
Start-Process -FilePath $installed
Write-Host "Lucent is installed at $installed"
