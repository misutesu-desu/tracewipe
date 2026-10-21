# Installs real programs, removes them with Nietono and checks that their
# folders are gone. Runs on GitHub's Windows runners and in Windows Sandbox.
#
#   pwsh tests/integration.ps1 -Exe target\release\nietono.exe
#
# Programs come from Chocolatey when it is installed, otherwise from their
# publishers' download pages. Run it only on a machine you can throw away:
# it installs and uninstalls software.

param(
    [Parameter(Mandatory)] [string] $Exe
)

$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path
$work = Join-Path $env:TEMP 'nietono-integration'
New-Item -ItemType Directory -Force $work | Out-Null

# One case per installer technology Nietono handles differently.
$cases = @(
    @{
        Name    = 'NSIS'
        Choco   = 'notepadplusplus.install'
        Url     = 'https://github.com/notepad-plus-plus/notepad-plus-plus/releases/download/v8.7.5/npp.8.7.5.Installer.x64.exe'
        Args    = '/S'
        Query   = 'notepad++'
        Folder  = "$env:ProgramFiles\Notepad++"
    },
    @{
        Name    = 'Inno Setup'
        Choco   = 'innosetup'
        Url     = 'https://jrsoftware.org/download.php/is.exe'
        Args    = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART'
        Query   = 'inno setup'
        Folder  = "${env:ProgramFiles(x86)}\Inno Setup 6"
    },
    @{
        Name    = 'MSI'
        Choco   = 'putty.install'
        Url     = 'https://the.earth.li/~sgtatham/putty/0.83/w64/putty-64bit-0.83-installer.msi'
        Args    = '/qn'
        Query   = 'putty'
        Folder  = "$env:ProgramFiles\PuTTY"
    }
)

function Invoke-Nietono([string[]] $Arguments) {
    # The release build is a GUI program, so wait for it explicitly and read its output from files.
    $out = Join-Path $work 'out.txt'
    $err = Join-Path $work 'err.txt'
    $p = Start-Process -FilePath $Exe -ArgumentList $Arguments -Wait -PassThru -NoNewWindow `
        -RedirectStandardOutput $out -RedirectStandardError $err
    Get-Content $out, $err | ForEach-Object { Write-Host "  $_" }
    return $p.ExitCode
}

function Install-Case($c) {
    if (Get-Command choco -ErrorAction SilentlyContinue) {
        choco install $c.Choco -y --no-progress | Out-Null
        return
    }
    $file = Join-Path $work ([IO.Path]::GetFileName(($c.Url -split '\?')[0]))
    if ($file -notmatch '\.(exe|msi)$') { $file += '.exe' }
    Invoke-WebRequest $c.Url -OutFile $file -UseBasicParsing
    if ($file.EndsWith('.msi')) {
        Start-Process msiexec.exe -ArgumentList "/i `"$file`" $($c.Args)" -Wait
    } else {
        Start-Process $file -ArgumentList $c.Args -Wait
    }
}

$failed = @()

Write-Host '== smoke'
if ((Invoke-Nietono @('--list')) -ne 0) { $failed += 'list' }
if ((Invoke-Nietono @('--orphans')) -ne 0) { $failed += 'orphans' }

foreach ($c in $cases) {
    Write-Host "== $($c.Name)"
    Install-Case $c
    if (-not (Test-Path $c.Folder)) {
        $failed += "$($c.Name): install did not create $($c.Folder)"
        continue
    }
    $code = Invoke-Nietono (@('--uninstall') + ($c.Query -split ' ') + @('--quiet', '--yes'))
    if ($code -ne 0) {
        $failed += "$($c.Name): nietono exited with $code"
    } elseif (Test-Path $c.Folder) {
        $failed += "$($c.Name): $($c.Folder) is still there"
    } else {
        Write-Host "  ok, $($c.Folder) is gone"
    }
}

# The cleanup must never touch the folders programs live in.
foreach ($p in @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramData, $env:APPDATA, $env:LOCALAPPDATA)) {
    if (-not (Test-Path $p)) { $failed += "protected folder $p is gone" }
}

if ($failed.Count -gt 0) {
    Write-Host '== failures'
    $failed | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host '== all passed'
