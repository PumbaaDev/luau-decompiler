# Upgrades AIKeepAlive to run at boot / with nobody logged in.
# SELF-ELEVATING: just run it normally, it will raise a UAC prompt itself.
# (Right-click "Run with PowerShell" does NOT elevate on its own, which is why
#  the first attempt silently failed with Access denied.)

$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

if (-not $isAdmin) {
    Write-Host "Not elevated - relaunching with a UAC prompt..." -ForegroundColor Yellow
    Start-Process powershell.exe -Verb RunAs -ArgumentList @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$PSCommandPath`""
    )
    return
}

Write-Host "Running elevated. Registering task..." -ForegroundColor Green
$script = "C:\Users\jep\Downloads\luau-decompiler-v2\tools\ai_keepalive.ps1"
$repo   = "C:\Users\jep\Downloads\luau-decompiler-v2"
$action = New-ScheduledTaskAction -Execute "powershell.exe" `
    -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$script`"" -WorkingDirectory $repo
$trig = @(
    (New-ScheduledTaskTrigger -AtStartup),
    (New-ScheduledTaskTrigger -AtLogOn),
    (New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(2) -RepetitionInterval (New-TimeSpan -Minutes 5))
)
$set  = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -MultipleInstances IgnoreNew -ExecutionTimeLimit (New-TimeSpan -Minutes 10)
$prin = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest

try {
    Register-ScheduledTask -TaskName "AIKeepAlive" -Action $action -Trigger $trig `
        -Settings $set -Principal $prin -Force -ErrorAction Stop | Out-Null
    $t = Get-ScheduledTask -TaskName "AIKeepAlive"
    Write-Host ""
    Write-Host "SUCCESS" -ForegroundColor Green
    Write-Host ("  runs as : {0} ({1})" -f $t.Principal.UserId, $t.Principal.LogonType)
    Write-Host ("  triggers: {0}" -f (($t.Triggers | ForEach-Object { $_.CimClass.CimClassName -replace 'MSFT_Task','' }) -join ', '))
} catch {
    Write-Host "FAILED: $($_.Exception.Message)" -ForegroundColor Red
}
Read-Host "`nPress Enter to close"
