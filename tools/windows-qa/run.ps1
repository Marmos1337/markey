# MarKey on a throw-away Windows machine. See .github/workflows/windows-qa.yml.
# Windows PowerShell 5.1: System.Drawing and Windows Forms are there by default.
param(
    [Parameter(Mandatory = $true)][string]$Installer,
    [Parameter(Mandatory = $true)][string]$Out,
    [Parameter(Mandatory = $true)][string]$Repo
)
$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force -Path $Out | Out-Null

$Here = Join-Path $Repo 'tools\windows-qa'
Add-Type -Path (Join-Path $Here 'QaNative.cs') -ReferencedAssemblies System.Drawing, System.Windows.Forms

$InstallDir = Join-Path $env:LOCALAPPDATA 'MarKey'
$Exe = Join-Path $InstallDir 'markey.exe'
$Data = Join-Path $env:APPDATA 'com.marmos.markey'
$Log = Join-Path $Data 'markey.log'
$Settings = Join-Path $Data 'settings.json'
$Mar = Join-Path $env:LOCALAPPDATA 'Mar'
$Report = Join-Path $Mar 'apps\markey.json'
$Sealed = Join-Path $Data 'secrets\account.sealed'
$UninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\MarKey'
$SchemeKey = 'HKCU:\Software\Classes\markey'
$RunKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$MockLog = Join-Path $Out 'mock.log'
$StateFile = Join-Path $Out 'target.state'
$Port = 8788

$script:Lines = New-Object System.Collections.ArrayList
$script:Failed = 0
$script:Passed = 0

function Check([string]$Name, [bool]$Ok, $Detail = '') {
    if ($Ok) { $script:Passed++ } else { $script:Failed++ }
    $mark = if ($Ok) { 'PASS' } else { 'FAIL' }
    $clean = ("$Detail" -replace '\s+', ' ').Trim()
    [void]$script:Lines.Add("| $mark | $Name | $($clean -replace '\|', '/') |")
    Write-Host "$mark  $Name  $clean"
}

function Note([string]$Text) {
    [void]$script:Lines.Add("| note | $Text | |")
    Write-Host "NOTE  $Text"
}

function Step([string]$Title, [scriptblock]$Body) {
    Write-Host "=== $Title"
    [void]$script:Lines.Add("| | **$Title** | |")
    try { & $Body } catch { Check "$Title stopped with an error" $false ($_ | Out-String) }
}

function Wait-Until([scriptblock]$Done, [int]$Seconds = 30, [int]$StepMs = 250) {
    $end = (Get-Date).AddSeconds($Seconds)
    do {
        try { if (& $Done) { return $true } } catch { }
        Start-Sleep -Milliseconds $StepMs
    } while ((Get-Date) -lt $end)
    return $false
}

function Read-Shared([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    # The file may be in the middle of being replaced.
    for ($try = 0; $try -lt 40; $try++) {
        try {
            $stream = [IO.File]::Open($Path, 'Open', 'Read', 'ReadWrite,Delete')
            try { return (New-Object IO.StreamReader($stream, [Text.Encoding]::UTF8)).ReadToEnd() } finally { $stream.Dispose() }
        } catch [IO.IOException] { Start-Sleep -Milliseconds 25 }
    }
    return ''
}

function Get-MarKey { return @(Get-Process -Name markey -ErrorAction SilentlyContinue) }

function Get-MainWindow {
    $process = Get-MarKey | Select-Object -First 1
    if (-not $process) { return $null }
    return [QaNative]::Windows($process.Id) | Where-Object { $_.Visible -and $_.Title -eq 'MarKey' -and $_.Width -gt 300 } | Select-Object -First 1
}

function Start-MarKey {
    $env:MARKEY_LOG = 'debug'
    $env:MARKEY_IDENTITY_URL = "http://127.0.0.1:$Port"
    return Start-Process -FilePath $Exe -PassThru
}

function Read-Report { return (Read-Shared $Report | ConvertFrom-Json) }

function Save-Shot([string]$Name) {
    $window = Get-MainWindow
    $screen = [QaNative]::ShotScreen((Join-Path $Out "$Name-screen.png"))
    if ($window) {
        $painted = [QaNative]::ShotWindow($window.Hwnd, (Join-Path $Out "$Name-window.png"))
        return [pscustomobject]@{ Window = $painted; Screen = $screen }
    }
    return [pscustomobject]@{ Window = $null; Screen = $screen }
}

function Target-Cmd([string]$Text) {
    [IO.File]::WriteAllText("$StateFile.cmd", $Text)
    [void](Wait-Until { -not (Test-Path -LiteralPath "$StateFile.cmd") } 5 50)
}

function Target-State {
    $state = @{}
    foreach ($line in (Read-Shared $StateFile) -split "`n") {
        $at = $line.IndexOf('=')
        if ($at -gt 0) { $state[$line.Substring(0, $at)] = $line.Substring($at + 1).Trim() }
    }
    foreach ($field in 'normal', 'password') {
        if ($state[$field] -ne $null) { $state[$field] = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($state[$field])) }
    }
    return $state
}

# Clicking gives the field real focus and the form the foreground, as a person would.
function Prepare([string]$Field, [string]$Layout) {
    Target-Cmd 'clear'
    $box = (Target-State).rect -split ','
    $y = [int]$box[1] + [int]($box[3] / 2)
    if ($Field -eq 'password') { $y += 50 }
    [QaNative]::Click([int]$box[0] + 300, $y)
    Target-Cmd "focus:$Field`nlayout:$Layout"
    [void](Wait-Until { $s = Target-State; $s.focus -eq $Field -and $s.layout -like "$Layout*" -and $s.normal -eq '' -and $s.password -eq '' } 6 80)
    Start-Sleep -Milliseconds 500
}

function Expect-Text([string]$Name, [string]$Field, [string]$Want) {
    $ok = Wait-Until { (Target-State)[$Field] -ceq $Want } 5 100
    Check $Name $ok "expected [$Want], got [$((Target-State)[$Field])]"
}

$mock = $null
$target = $null

Step 'The machine' {
    $os = Get-CimInstance Win32_OperatingSystem
    Note "$($os.Caption) build $($os.BuildNumber)"
    $view = Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -ErrorAction SilentlyContinue
    Note "WebView2 $($view.pv)"
    Add-Type -AssemblyName System.Windows.Forms
    Note "Screen $([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Size)"
    Note ("Installer " + (Split-Path $Installer -Leaf) + ", sha256 " + (Get-FileHash $Installer -Algorithm SHA256).Hash.ToLower())
}

Step 'The mock server' {
    $script:mock = Start-Process python -ArgumentList @('-u', 'tools\mock_identity.py', '--port', "$Port", '--open-return') -WorkingDirectory $Repo -RedirectStandardOutput $MockLog -RedirectStandardError (Join-Path $Out 'mock.err') -PassThru -WindowStyle Hidden
    Check 'Mock server listening' (Wait-Until { (Read-Shared $MockLog) -match 'mock identity on' } 15)
}

Step 'Keyboard layouts and browser' {
    # The two layouts a person who needs MarKey has.
    $languages = Get-WinUserLanguageList
    $languages.Add('ru-RU')
    Set-WinUserLanguageList -LanguageList $languages -Force
    Check 'Russian keyboard layout loaded' ([QaNative]::LoadRussian())
    # No question from the browser's first run in front of the sign-in test.
    New-Item -Path 'HKCU:\Software\Policies\Microsoft\Edge' -Force | Out-Null
    Set-ItemProperty -Path 'HKCU:\Software\Policies\Microsoft\Edge' -Name HideFirstRunExperience -Value 1 -Type DWord
}

Step 'Silent install /S' {
    $run = Start-Process -FilePath $Installer -ArgumentList '/S' -Wait -PassThru
    Check 'Installer exit code 0' ($run.ExitCode -eq 0) "exit $($run.ExitCode)"
    Check 'markey.exe installed for this user' (Test-Path -LiteralPath $Exe) $Exe
    Check 'Models installed' ((Get-ChildItem (Join-Path $InstallDir 'models') -Filter *.rkm -ErrorAction SilentlyContinue).Count -ge 10)
    Check 'Nothing written to Program Files' (-not (Test-Path 'C:\Program Files\MarKey'))
    $key = Get-ItemProperty -Path $UninstallKey -ErrorAction SilentlyContinue
    Check 'DisplayName' ($key.DisplayName -eq 'MarKey') $key.DisplayName
    Check 'Publisher' ($key.Publisher -eq 'Marmos') $key.Publisher
    Check 'DisplayVersion' ($key.DisplayVersion -match '^\d+\.\d+\.\d+$') $key.DisplayVersion
    Check 'InstallLocation' ("$($key.InstallLocation)".Trim('"') -eq $InstallDir) $key.InstallLocation
    Check 'UninstallString' ([bool]$key.UninstallString) $key.UninstallString
    Note "QuietUninstallString: $($key.QuietUninstallString)"
    $command = (Get-ItemProperty -Path "$SchemeKey\shell\open\command" -ErrorAction SilentlyContinue).'(default)'
    Check 'markey:// points at the installed program' ("$command" -like "*$Exe*%1*") $command
    Check 'markey:// is a URL protocol' ($null -ne (Get-ItemProperty -Path $SchemeKey -Name 'URL Protocol' -ErrorAction SilentlyContinue))
}

Step 'First launch: the window paints' {
    [void](Start-MarKey)
    Check 'Window appears' (Wait-Until { Get-MainWindow } 60)
    $seen = Wait-Until {
        $script:shot = Save-Shot '01-first-launch'
        $script:shot.Screen.Distinct -ge 64 -and $script:shot.Window -and $script:shot.Window.Distinct -ge 64
    } 45 1500
    $window = $script:shot.Window
    Check 'Settings window painted (PrintWindow)' $seen "distinct colours $($window.Distinct), variance $([math]::Round($window.Variance, 1)), size $($window.Width)x$($window.Height)"
    Check 'One markey.exe' ((Get-MarKey).Count -eq 1) "count $((Get-MarKey).Count)"
    $web = @(Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" | Where-Object { $_.CommandLine -match 'com\.marmos\.markey|MarKey|markey' })
    Check 'WebView2 runs for it' ($web.Count -gt 0) "processes $($web.Count)"
    Check 'Log file written' (Wait-Until { (Read-Shared $Log).Length -gt 0 } 10) $Log
    Check 'Silent sign-in stays quiet with no MarHub' (Wait-Until { (Read-Shared $Log) -match 'silent sign-in: MarHub is not available' } 15)
    Check 'Report for MarHub written' (Wait-Until { Test-Path -LiteralPath $Report } 15) $Report
    $json = Read-Report
    Check 'Report: schema, app, version' ($json.schema -eq 1 -and $json.app -eq 'markey' -and $json.version -match '^\d+\.\d+\.\d+$') "schema $($json.schema), app $($json.app), version $($json.version)"
    Check 'Report: deep link' ($json.deepLink -eq 'markey://') $json.deepLink
    Check 'Report: signed out' ($json.account.signedIn -eq $false)
    Check 'Report: no token words' (-not ((Read-Shared $Report) -match 'token|verifier|password'))
    Copy-Item -LiteralPath $Report -Destination (Join-Path $Out 'markey.json') -ErrorAction SilentlyContinue
    Note "Permissions in the report: $($json.permissions | ConvertTo-Json -Compress)"
    Check 'A new installation starts at login' (Wait-Until { (Get-ItemProperty -Path $RunKey -ErrorAction SilentlyContinue).MarKey } 15) (Get-ItemProperty -Path $RunKey -ErrorAction SilentlyContinue).MarKey
}

Step 'The keyboard hook corrects real typing' {
    $main = Get-MainWindow
    if ($main) { [QaNative]::Close($main.Hwnd) }
    Check 'Closing the window hides it and MarKey keeps running' ((Wait-Until { -not (Get-MainWindow) } 10) -and (Get-MarKey).Count -eq 1)
    $exe = Join-Path $Out 'target.exe'
    $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    & $csc /nologo /target:winexe "/out:$exe" /r:System.Windows.Forms.dll /r:System.Drawing.dll (Join-Path $Here 'Target.cs') | Out-Null
    Check 'Test window built' (Test-Path -LiteralPath $exe)
    $script:target = Start-Process -FilePath $exe -ArgumentList "`"$StateFile`"" -PassThru
    Check 'Test window running' (Wait-Until { (Target-State).hwnd } 15)

    Prepare 'normal' 'en'
    [QaNative]::Type('ghbdtn ')
    Expect-Text 'ghbdtn + space becomes привет' 'normal' 'привет '
    Check 'Layout switched to Russian in the window being typed in' (Wait-Until { (Target-State).layout -like 'ru*' } 4) (Target-State).layout
    [void](Save-Shot '02-corrected')

    [QaNative]::Tap(0xA3, $true)
    Expect-Text 'Right Ctrl undoes the correction' 'normal' 'ghbdtn '

    Prepare 'normal' 'en'
    [QaNative]::Type('hello ')
    Expect-Text 'A correct English word is left alone' 'normal' 'hello '

    Prepare 'normal' 'en'
    [QaNative]::Type('Ghjtrn ')
    Expect-Text 'Shift for a capital does not break the word' 'normal' 'Проект '

    Prepare 'normal' 'en'
    [QaNative]::Type('nen')
    Start-Sleep -Milliseconds 300
    [QaNative]::Tap(0xA3, $true)
    Expect-Text 'Right Ctrl converts a word that was not corrected' 'normal' 'тут'

    Prepare 'normal' 'ru'
    [QaNative]::Tap(0x14, $false)
    [void](Wait-Until { (Target-State).caps -eq '1' } 3 80)
    [QaNative]::Type('Ghjtrn ')
    Expect-Text 'Caps Lock left on: inverted word is fixed' 'normal' 'Проект '
    Check 'Caps Lock is switched off' (Wait-Until { (Target-State).caps -eq '0' } 4 80) "caps $((Target-State).caps)"
    if ((Target-State).caps -eq '1') { [QaNative]::Tap(0x14, $false) }

    Prepare 'password' 'en'
    Start-Sleep -Milliseconds 400
    [QaNative]::Type('ghjtrn ')
    Expect-Text 'A password field is left alone' 'password' 'ghjtrn '

    Prepare 'normal' 'en'
    [QaNative]::Type('ghjtrn')
    $box = (Target-State).rect -split ','
    [QaNative]::Click([int]$box[0] + 4, [int]$box[1] + [int]($box[3] / 2))
    Start-Sleep -Milliseconds 300
    [QaNative]::Type(' ')
    Start-Sleep -Milliseconds 900
    $text = (Target-State).normal
    # The space lands wherever the click put the caret; the letters must stay Latin.
    Check 'A click stops tracking: nothing is rewritten' (($text -replace ' ', '') -ceq 'ghjtrn') "got [$text]"

    Set-Clipboard -Value 'QA-SENTINEL'
    Prepare 'normal' 'en'
    [QaNative]::Type('hello')
    Start-Sleep -Milliseconds 200
    [QaNative]::Key(0xA2, $true, $false) | Out-Null
    [QaNative]::Tap(0x41, $false)
    [QaNative]::Key(0xA2, $false, $false) | Out-Null
    Start-Sleep -Milliseconds 300
    [QaNative]::Tap(0xA3, $true)
    Expect-Text 'Right Ctrl converts the selection' 'normal' 'руддщ'
    Check 'The clipboard comes back' (Wait-Until { (Get-Clipboard) -eq 'QA-SENTINEL' } 5 200) "clipboard [$(Get-Clipboard)]"
}

Step 'markey:// links through the system' {
    $count = (Get-MarKey).Count
    $rotated = (Get-Item -LiteralPath "$Log.1" -ErrorAction SilentlyContinue).LastWriteTimeUtc
    $from = (Read-Shared $Log).Length
    Start-Process 'markey://auth?code=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&state=BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB'
    $ignored = Wait-Until { (Read-Shared $Log).Substring($from) -match 'matched no waiting attempt' } 20
    Check 'A forged sign-in link reaches the running copy and is ignored' $ignored
    Check 'It opens no window' (-not (Get-MainWindow))
    Check 'The second process hands over and leaves' (Wait-Until { (Get-MarKey).Count -eq $count } 15) "count $((Get-MarKey).Count)"
    $from = (Read-Shared $Log).Length
    Start-Process 'markey://auth?code=x&code=y&state=z'
    Check 'A malformed link is ignored' (Wait-Until { (Read-Shared $Log).Substring($from) -match 'not valid' } 15)
    Start-Process 'markey://'
    Check 'A bare markey:// shows the settings' (Wait-Until { Get-MainWindow } 20)
    Check 'Still one markey.exe' (Wait-Until { (Get-MarKey).Count -eq $count } 15) "count $((Get-MarKey).Count)"
    Check 'A link does not rotate the log of the running copy' ((Get-Item -LiteralPath "$Log.1" -ErrorAction SilentlyContinue).LastWriteTimeUtc -eq $rotated)
    Check 'The log holds no link contents' (-not ((Read-Shared $Log) -match 'AAAAAAAA|BBBBBBBB|code='))
}

Step 'Sign-in, DPAPI and the cloud against the local mock' {
    $window = Get-MainWindow
    if (-not $window) { [void](Start-Process 'markey://'); [void](Wait-Until { Get-MainWindow } 15); $window = Get-MainWindow }
    [void](Save-Shot '03-before-sign-in')
    $origin = [QaNative]::ClientOrigin($window.Hwnd)
    [QaNative]::Click($origin[0] + 106, $origin[1] + 507)
    $exchanged = Wait-Until { (Read-Shared $MockLog) -match 'exchange 200' } 60 500
    [void](Save-Shot '04-after-sign-in')
    Check 'The sign-in button leads through the browser to a code exchange' $exchanged
    Check 'MarKey reports it is signed in' (Wait-Until { (Read-Report).account.signedIn -eq $true } 20) (Read-Shared $Report)
    Check 'Log says so' ((Read-Shared $Log) -match 'signed in \(silent: false\)')
    Check 'Session sealed on disk' (Test-Path -LiteralPath $Sealed) $Sealed
    if (Test-Path -LiteralPath $Sealed) {
        $raw = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($Sealed))
        Check 'The sealed file shows no token field' (-not ($raw -match 'refreshToken|accessToken|mock')) "size $((Get-Item -LiteralPath $Sealed).Length) bytes"
    }
    Check 'Rules are uploaded to the cloud' (Wait-Until { (Read-Shared $MockLog) -match 'config/rules 200' } 20) ((Read-Shared $MockLog) -split "`n" | Select-Object -Last 8)
    Check 'Exception words are not uploaded by default' (-not ((Read-Shared $MockLog) -match 'word-exceptions'))

    $old = (Get-MarKey | Select-Object -First 1).Id
    $seen = ((Read-Shared $MockLog) -split "`n").Count
    Stop-Process -Id $old -Force
    Check 'MarKey is killed' (Wait-Until { (Get-MarKey).Count -eq 0 } 10)
    $valid = $true
    try { [void]((Read-Shared $Settings) | ConvertFrom-Json) } catch { $valid = $false }
    Check 'Settings are intact after the kill' $valid
    [void](Start-MarKey)
    $restored = Wait-Until { (Read-Report).account.signedIn -eq $true -and (((Read-Shared $MockLog) -split "`n").Count -gt $seen) } 30 500
    Check 'After the kill the session comes back from DPAPI' $restored
    Check 'No complaint about the stored session' (-not ((Read-Shared $Log) -match 'stored account session cannot be read'))
    Copy-Item -LiteralPath $MockLog -Destination (Join-Path $Out 'mock-after-sign-in.log') -ErrorAction SilentlyContinue
}

Step 'Update over a running copy' {
    $old = (Get-MarKey | Select-Object -First 1).Id
    $run = Start-Process -FilePath $Installer -ArgumentList '/S' -Wait -PassThru
    Check 'Installer exit code 0 with MarKey running' ($run.ExitCode -eq 0) "exit $($run.ExitCode)"
    Check 'The running copy was closed' (-not (Get-Process -Id $old -ErrorAction SilentlyContinue))
    Check 'markey.exe still in place' (Test-Path -LiteralPath $Exe)
    [void](Wait-Until { (Get-MarKey).Count -eq 0 } 10)
    [void](Start-MarKey)
    Check 'Starts again after the update' (Wait-Until { (Get-MarKey).Count -eq 1 } 20)
    Check 'Still signed in after the update' (Wait-Until { (Read-Report).account.signedIn -eq $true } 30)
}

Step 'Silent uninstall /S and what stays on disk' {
    $old = (Get-MarKey | Select-Object -First 1).Id
    $uninstaller = Join-Path $InstallDir 'uninstall.exe'
    Check 'Uninstaller present' (Test-Path -LiteralPath $uninstaller) $uninstaller
    $run = Start-Process -FilePath $uninstaller -ArgumentList '/S' -Wait -PassThru
    Note "Uninstaller exit code $($run.ExitCode)"
    Check 'Program files are removed' (Wait-Until { -not (Test-Path -LiteralPath $Exe) } 60)
    Check 'MarKey is not left running' (Wait-Until { (Get-MarKey).Count -eq 0 } 20)
    Check 'Uninstall entry removed' (-not (Test-Path $UninstallKey))
    Check 'markey:// registration removed' (-not (Test-Path $SchemeKey))
    $autostart = (Get-ItemProperty -Path $RunKey -ErrorAction SilentlyContinue).MarKey
    Check 'Autostart entry removed' (-not $autostart) $autostart
    Check 'Settings stay' (Test-Path -LiteralPath $Settings)
    Check 'The sealed session stays' (Test-Path -LiteralPath $Sealed)
    Check 'The report stays' (Test-Path -LiteralPath $Report)
    $left = @()
    foreach ($root in $InstallDir, $Data, $Mar) {
        $left += "== $root"
        if (Test-Path -LiteralPath $root) {
            $left += Get-ChildItem -LiteralPath $root -Recurse -Force | ForEach-Object { "$($_.FullName.Substring($root.Length))  $($_.Length)" }
        } else { $left += '(gone)' }
    }
    $left | Set-Content -LiteralPath (Join-Path $Out 'left-on-disk.txt') -Encoding UTF8
    Note ("Left on disk: " + (($left | Select-Object -First 24) -join '; '))
}

if ($script:target) { Stop-Process -Id $script:target.Id -Force -ErrorAction SilentlyContinue }
if ($script:mock) { Stop-Process -Id $script:mock.Id -Force -ErrorAction SilentlyContinue }
Copy-Item -LiteralPath $Log -Destination (Join-Path $Out 'markey.log') -ErrorAction SilentlyContinue

$summary = @("## Windows QA: $($script:Passed) passed, $($script:Failed) failed", '', '| | Check | Detail |', '|---|---|---|') + $script:Lines
$summary | Set-Content -LiteralPath (Join-Path $Out 'report.md') -Encoding UTF8
Write-Host "Passed $($script:Passed), failed $($script:Failed)"
if ($script:Failed -gt 0) { exit 1 }
exit 0
