[CmdletBinding()]
param(
    [ValidateSet('Check', 'Build', 'Run')]
    [string]$Action = 'Check',

    [ValidateSet('release', 'diag')]
    [string]$Profile = 'release',

    [ValidateSet('series-x', 'series-s')]
    [string]$ConsoleProfile = 'series-x',

    [ValidateRange(1, 64)]
    [int]$Jobs = 4,

    [ValidateRange(0, 1048576)]
    [int]$ShaderLoopLimit = 0,

    # The title's output size: written into its settings file and, when it is one of the title's modes
    # (1280x720, 1664x936, 1920x1080, 2560x1440, 3840x2160), patched into its code at load (VICTORY_OUTPUT),
    # since on Series X it renders 3840x2160 whatever the file says. 'keep' leaves both alone.
    [ValidatePattern('^(keep|\d{3,4}x\d{3,4})$')]
    [string]$OutputSize = '1920x1080',

    # The scene's share of the output's width and height, 0.5 to 1.0 (VICTORY_RENDER_SCALE). 0, the default,
    # leaves the title's own 2/3: a 1280x720 scene at 1920x1080; 1.0 renders 1920x1080.
    [ValidateScript({ $_ -eq 0 -or ($_ -ge 0.5 -and $_ -le 1.0) })]
    [double]$RenderScale = 0,

    # Leaves the title's code as it is: no output size, render scale or upscaler patched (the settings file
    # is still written).
    [switch]$NoTitlePatch,

    # 'light' turns off what costs most on a PC card, 'keep' changes nothing, 'original' restores the title's file.
    [ValidateSet('light', 'keep', 'original')]
    [string]$TitleSettings = 'light',

    [string[]]$TitleArguments = @(),

    [switch]$NoDebugWindow,

    # Build: let Cargo use the network to fetch crates missing from Vyce-data\cargo. Without it the build is
    # offline (CARGO_NET_OFFLINE=true) and takes the crates Cargo.lock names from that folder only.
    [switch]$Online
)

$ErrorActionPreference = 'Stop'
$Profile = $Profile.ToLowerInvariant()
$ConsoleProfile = $ConsoleProfile.ToLowerInvariant()
$repo = Split-Path -Parent $PSScriptRoot
$state = Join-Path (Split-Path -Parent $repo) ((Split-Path -Leaf $repo) + '-data')
$config = Join-Path $repo 'victory.toml'
$target = Join-Path $state 'target'
$logDir = Join-Path $state 'logs'
$monitor = Join-Path $PSScriptRoot 'vyce-monitor.ps1'
$monitorResources = Join-Path $PSScriptRoot 'vyce-monitor-resources.ps1'
$showMonitor = $Action -eq 'Run' -and -not $NoDebugWindow -and [Environment]::UserInteractive
$rust = Join-Path $state 'toolchain\rust'
$cargo = Join-Path $rust 'bin\cargo.exe'
$rustc = Join-Path $rust 'bin\rustc.exe'
$linker = Join-Path $rust 'lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe'
$libs = @('toolchain\crt', 'toolchain\sdk\um', 'toolchain\sdk\ucrt') | ForEach-Object {
    Join-Path $state $_
}
$binary = Join-Path $target "$Profile\victory.exe"
$dependencies = @(
    $cargo
    $rustc
    $linker
    (Join-Path $libs[0] 'libcmt.lib')
    (Join-Path $libs[1] 'kernel32.lib')
    (Join-Path $libs[2] 'libucrt.lib')
)
$missing = @($dependencies | Where-Object { -not (Test-Path -LiteralPath $_ -PathType Leaf) })
$temp = Join-Path $state 'temp'
$cache = Join-Path $state 'cache'
$data = Join-Path $state 'data'

if ($Action -eq 'Check') {
    Write-Host "Vyce:       $repo"
    Write-Host "State:      $state"
    Write-Host "Config:     $config"
    Write-Host "Build:      $binary"
    Write-Host "Cache:      $cache"
    Write-Host "Saves:      $data"
    Write-Host "Logs:       $logDir"
    Write-Host "Temporary:  $temp"
    Write-Host "Debug panel: $monitor"
    if ($missing) { $missing | ForEach-Object { Write-Warning "Missing build dependency: $_" } }
    if (-not (Test-Path -LiteralPath $config -PathType Leaf)) { Write-Warning 'Local victory.toml is missing.' }
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { Write-Host 'Selected build is not compiled yet.' }
    return
}

# The title's own settings file (plain XML). The first copy of it is kept beside it.
function Find-TitleSettings {
    $pattern = Join-Path $data '*\storage\settings\settings.xml'
    Get-ChildItem -Path $pattern -File -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName
}

function Read-TitleSettings([string]$Path) {
    $document = New-Object System.Xml.XmlDocument
    $document.PreserveWhitespace = $true
    $document.Load($Path)
    $document
}

function Write-TitleSettings([string]$Path, [System.Xml.XmlDocument]$Document) {
    $writer = [IO.StreamWriter]::new($Path, $false, [Text.UTF8Encoding]::new($false))
    try { $Document.Save($writer) } finally { $writer.Dispose() }
}

# One settings file per console: the title writes its card's name into it.
function Switch-TitleConsole([string]$Path, [string]$Console) {
    $card = if ($Console -eq 'series-s') { 'Xbox Series S' } else { 'Xbox Series X' }
    $current = (Read-TitleSettings $Path).DocumentElement.SelectSingleNode('videoCardDescription')
    if ($null -eq $current -or $current.InnerText -eq $card) { return $true }
    $folder = Split-Path -Parent $Path
    Move-Item -LiteralPath $Path -Destination (Join-Path $folder "settings.$($current.InnerText).xml") -Force
    $wanted = Join-Path $folder "settings.$card.xml"
    if (-not (Test-Path -LiteralPath $wanted -PathType Leaf)) { return $false }
    Copy-Item -LiteralPath $wanted -Destination $Path
    $true
}

function Update-TitleSettings {
    $file = Find-TitleSettings
    if (-not $file) {
        Write-Host 'Title settings: none yet (the title writes them on its first start).'
        return
    }
    try {
        if (-not (Switch-TitleConsole $file $ConsoleProfile)) {
            Write-Host "Title settings: none for $ConsoleProfile yet."
            return
        }
        $card = if ($ConsoleProfile -eq 'series-s') { 'Xbox Series S' } else { 'Xbox Series X' }
        $original = Join-Path (Split-Path -Parent $file) "settings.$card.original.xml"
        if (-not (Test-Path -LiteralPath $original -PathType Leaf)) { Copy-Item -LiteralPath $file -Destination $original }
        if ($TitleSettings -eq 'original') {
            Copy-Item -LiteralPath $original -Destination $file -Force
        }
        $values = [ordered]@{}
        if ($TitleSettings -ne 'original' -and $OutputSize -ne 'keep') {
            $width, $height = $OutputSize -split 'x'
            $values['video/resolutionFullscreen/width'] = $width
            $values['video/resolutionFullscreen/height'] = $height
        }
        if ($TitleSettings -eq 'light') {
            # One host queue runs everything: the title's other queues only add waits.
            $values['advancedGraphics/asyncComputeEnabled'] = 'false'
            $values['advancedGraphics/transferQueuesEnabled'] = 'false'
            # A smear at low frame rates.
            $values['advancedGraphics/motionBlur'] = 'false'
            # Fewer draws.
            $values['advancedGraphics/lodScale'] = '0.700000'
            $values['advancedGraphics/grassLod'] = '0.500000'
            $values['advancedGraphics/smallCullThreshold'] = '2.000000'
            $values['advancedGraphics/vehicleHDCount'] = '1'
            $values['advancedGraphics/ssroSampleCount'] = '1'
            $values['advancedGraphics/waterReflectionResX'] = '256'
            $values['advancedGraphics/waterReflectionResY'] = '128'
            $values['advancedGraphics/waterReflectionSSR'] = 'false'
            $values['advancedGraphics/lensFlaresScreenResDivExponent'] = '3'
        }
        if ($values.Count -gt 0) {
            $document = Read-TitleSettings $file
            $changed = @()
            foreach ($key in $values.Keys) {
                $node = $document.DocumentElement.SelectSingleNode($key)
                if ($null -eq $node -or -not $node.HasAttribute('value')) {
                    Write-Warning "Title settings: no '$key' in the file."
                } elseif ($node.GetAttribute('value') -ne $values[$key]) {
                    $node.SetAttribute('value', $values[$key])
                    $changed += "$(Split-Path -Leaf $key)=$($values[$key])"
                }
            }
            if ($changed) {
                Write-TitleSettings $file $document
                Write-Host "Title settings changed: $($changed -join ', ')"
            }
        }
        Copy-Item -LiteralPath $file -Destination (Join-Path $logDir "$stamp.settings.xml")
    } catch {
        Write-Warning "Title settings left as they are: $_"
    }
}

if ($Action -eq 'Build' -and $missing) { throw ("Missing portable build dependencies: " + ($missing -join ', ')) }
if ($Action -eq 'Run') {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Build the $Profile profile first." }
    if (-not (Test-Path -LiteralPath $config -PathType Leaf)) { throw "Configure your game folder in $config first." }
}

New-Item -ItemType Directory -Path $state, $target, $logDir, $temp, $cache, $data -Force | Out-Null
$settings = @{
    CARGO_HOME = Join-Path $state 'cargo'
    CARGO_TARGET_DIR = $target
    CARGO_BUILD_BUILD_DIR = $target
    RUSTUP_HOME = Join-Path $state 'rustup'
    TEMP = $temp
    TMP = $temp
    VICTORY_CONFIG = $config
    VICTORY_GAME_DIR = $null
    VICTORY_CACHE_DIR = $cache
    VICTORY_DATA_DIR = $data
}
if ($showMonitor) {
    $settings.VYCE_TELEMETRY = '1'
}
if ($Action -eq 'Run' -and $ShaderLoopLimit -gt 0) {
    $settings.VICTORY_SHADER_LOOP_LIMIT = [string]$ShaderLoopLimit
}
# The title's output size, render scale and upscaler, patched into its code at load after its bytes are
# compared (build 1.0.2.1; another build is left alone, said so in the log). A value the caller's
# environment already gives wins (VICTORY_OUTPUT=off leaves the size alone).
$titleModes = @('1280x720', '1664x936', '1920x1080', '2560x1440', '3840x2160')
if ($Action -eq 'Run' -and -not $NoTitlePatch) {
    $patch = [ordered]@{}
    if ($OutputSize -ne 'keep') {
        if ($titleModes -contains $OutputSize) {
            $patch.VICTORY_OUTPUT = $OutputSize.ToLowerInvariant()
        } else {
            Write-Warning "-OutputSize $OutputSize is not one of the title's modes ($($titleModes -join ', ')): only its settings file gets it."
        }
    }
    if ($RenderScale -gt 0) {
        $patch.VICTORY_RENDER_SCALE = $RenderScale.ToString([Globalization.CultureInfo]::InvariantCulture)
    }
    if ($ConsoleProfile -eq 'series-s') {
        # The title's own upscaler: on Series S the title picks FidelityFX, which other NVIDIA forks saw
        # draw a black world.
        $patch.VICTORY_UPSCALER = '6'
    }
    foreach ($key in $patch.Keys) {
        $given = [Environment]::GetEnvironmentVariable($key, 'Process')
        if ($given) {
            Write-Host "Title patch: $key=$given, as the caller set it."
        } else {
            $settings[$key] = $patch[$key]
            Write-Host "Title patch: $key=$($patch[$key])"
        }
    }
}

if ($Action -eq 'Build') {
    $settings.RUSTC = $rustc
    $settings.LIB = $libs -join ';'
    # Offline unless -Online: nothing is downloaded, whatever the caller's environment or Cargo's own
    # configuration says.
    $settings.CARGO_NET_OFFLINE = if ($Online) { 'false' } else { 'true' }
    $settings.CARGO_ENCODED_RUSTFLAGS = @(
        '-C', 'target-feature=+crt-static'
        '-C', 'linker-flavor=lld-link'
        '-C', "linker=$linker"
        '-C', 'link-arg=/ignore:4099'
    ) -join [char]31
}
$previous = @{}
try {
    foreach ($key in $settings.Keys) {
        $previous[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
        [Environment]::SetEnvironmentVariable($key, $settings[$key], 'Process')
    }

    if ($Action -eq 'Build') {
        $features = if ($Profile -eq 'diag') { 'diagnostics,blake3/pure' } else { 'blake3/pure' }
        $network = if ($Online) { 'online (-Online)' } else { 'offline, crates from Vyce-data\cargo only (-Online to download)' }
        Write-Host "Build: $network"
        Push-Location -LiteralPath $repo
        try {
            & $cargo build --locked --profile $Profile -p victory --features $features --jobs $Jobs
            if ($LASTEXITCODE -ne 0) {
                if (-not $Online) { Write-Warning 'An offline build stops at a crate missing from Vyce-data\cargo; -Online lets Cargo fetch it.' }
                throw "Cargo exited with code $LASTEXITCODE."
            }
        } finally {
            Pop-Location
        }
        Write-Host "Built: $binary"
        return
    }

    $stamp = (Get-Date -Format 'yyyy-MM-dd_HH-mm-ss-fff') + "-$PID"
    if (Get-Process -Name victory -ErrorAction SilentlyContinue) {
        Write-Warning 'Another victory.exe is running: the title settings are left as they are.'
    } else {
        Update-TitleSettings
    }
    $stdout = Join-Path $logDir "$stamp.log"
    $stderr = Join-Path $logDir "$stamp.err.log"
    Write-Host "Logs: $stdout and $stderr"
    $launch = @{
        FilePath = $binary
        WorkingDirectory = $state
        ArgumentList = @('run', '--console', $ConsoleProfile, '--execute')
        RedirectStandardOutput = $stdout
        RedirectStandardError = $stderr
        NoNewWindow = $true
        PassThru = $true
    }
    if ($TitleArguments) { $launch.ArgumentList += @('--') + $TitleArguments }
    $process = Start-Process @launch
    if ($showMonitor) {
        try {
            foreach ($scriptFile in @($monitor, $monitorResources)) {
                if (-not (Test-Path -LiteralPath $scriptFile -PathType Leaf)) {
                    throw "Monitor script not found: $scriptFile"
                }
            }
            $shell = if ($PSVersionTable.PSEdition -eq 'Core') { 'pwsh.exe' } else { 'powershell.exe' }
            $monitorLog = Join-Path $logDir "$stamp.monitor.log"
            $monitorArgs = @(
                '-NoLogo', '-NoProfile', '-NonInteractive', '-STA', '-File', ('"' + $monitor + '"')
                '-GamePid', [string]$process.Id
                '-StdoutLog', ('"' + $stdout + '"')
                '-StderrLog', ('"' + $stderr + '"')
            )
            Start-Process -FilePath (Join-Path $PSHOME $shell) -WorkingDirectory $state -NoNewWindow -ArgumentList $monitorArgs -RedirectStandardError $monitorLog
        } catch {
            Write-Warning "Could not open the Vyce debug panel. The game is still running. $_"
        }
    }
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) { throw "Vyce exited with code $($process.ExitCode). See $stderr." }
} finally {
    foreach ($key in $previous.Keys) {
        [Environment]::SetEnvironmentVariable($key, $previous[$key], 'Process')
    }
}
