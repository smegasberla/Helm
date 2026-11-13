<#
.SYNOPSIS
    Register the Helm helper as a Windows service.

.DESCRIPTION
    The helper needs the rights to read the block devices and to write the
    firmware variables, so it runs as the local system account. The interface
    talks to it over the named pipe \\.\pipe\helm-helper.

    Run this script from an elevated PowerShell window:

        powershell -ExecutionPolicy Bypass -File packaging\windows\install-service.ps1

.PARAMETER Binary
    The path of the helper. The default is the one that the MSI installs.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\windows\install-service.ps1 -Binary C:\Program Files\Helm\helm-helper.exe
#>
param(
    [string]$Binary = "$env:ProgramFiles\Helm\helm-helper.exe",
    [string]$ServiceName = "HelmHelper",
    [string]$DisplayName = "Helm Helper"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Binary)) {
    Write-Error "the helper was not found at $Binary"
    exit 1
}

$service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($service) {
    Write-Host "the service $ServiceName already exists, it is updated"
    if ($service.Status -eq "Running") {
        Stop-Service -Name $ServiceName -Force
    }
    sc.exe config $ServiceName binPath= "`"$Binary`" --serve" start= auto obj= LocalSystem | Out-Null
} else {
    Write-Host "creating the service $ServiceName"
    sc.exe create $ServiceName binPath= "`"$Binary`" --serve" start= auto obj= LocalSystem DisplayName= "`"$DisplayName`"" | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Error "the service could not be created"
        exit 1
    }
    sc.exe description $ServiceName "Helm helper: finds the installed systems and switches the boot order" | Out-Null
    sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/10000/restart/60000 | Out-Null
}

Write-Host "starting the service"
Start-Service -Name $ServiceName
Get-Service -Name $ServiceName | Format-Table -AutoSize

Write-Host ""
Write-Host "The helper is installed. The interface can be started with 'helm-gui'."
