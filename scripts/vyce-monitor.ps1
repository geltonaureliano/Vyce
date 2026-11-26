#requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateRange(1, 2147483647)][int]$GamePid,
    [Parameter(Mandatory)][string]$StdoutLog,
    [Parameter(Mandatory)][string]$StderrLog
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

# Loads the panel's C#. Compiling takes seconds beside a game that is
# starting, so the result is kept in the temporary folder, named by the text
# it comes from.
function Import-Panel {
    . (Join-Path $PSScriptRoot 'vyce-monitor-model.ps1')
    . (Join-Path $PSScriptRoot 'vyce-monitor-view.ps1')
    $source = $VyceMonitorModel + "`n" + $VyceMonitorView
    # The sampler's few lines of C# go along (first, for their usings): it
    # finds its type made and compiles nothing.
    $sampler = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'vyce-monitor-resources.ps1'))
    if ($sampler -match '(?s)-TypeDefinition @"\s*\n(.+?)\n"@') { $source = $Matches[1] + "`n" + $source }
    $references = 'System.Windows.Forms', 'System.Drawing', 'System.Core'
    $sha = [Security.Cryptography.SHA256]::Create()
    $bytes = $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes("$([Environment]::Version) $source"))
    $sha.Dispose()
    $name = 'vyce-monitor-' + [BitConverter]::ToString($bytes, 0, 12).Replace('-', '').ToLowerInvariant()
    $folder = [IO.Path]::GetTempPath()
    $kept = Join-Path $folder "$name.dll"
    if (Test-Path -LiteralPath $kept -PathType Leaf) {
        try {
            # From its bytes, so the file is never in use.
            [void][Reflection.Assembly]::Load([IO.File]::ReadAllBytes($kept))
            return
        } catch {
            Remove-Item -LiteralPath $kept -Force -ErrorAction SilentlyContinue
        }
    }
    $made = Join-Path $folder "$name-$PID.dll"
    try {
        Add-Type -TypeDefinition $source -ReferencedAssemblies $references -IgnoreWarnings -OutputAssembly $made -OutputType Library
        [void][Reflection.Assembly]::Load([IO.File]::ReadAllBytes($made))
    } catch {
        # A folder that takes no file, or a compiler that fails: the second
        # try tells which.
        if (-not ('Vyce.Monitor.PanelForm' -as [type])) {
            Add-Type -TypeDefinition $source -ReferencedAssemblies $references -IgnoreWarnings
        }
        return
    }
    try {
        Get-ChildItem -LiteralPath $folder -Filter 'vyce-monitor-*.dll' |
            Where-Object { $_.FullName -ne $made } |
            Remove-Item -Force -ErrorAction SilentlyContinue
        Move-Item -LiteralPath $made -Destination $kept -Force
    } catch {
        # Compiled again next time.
    }
}

function Get-Known($Value) {
    if ($null -eq $Value) { [double]::NaN } else { [double]$Value }
}

function Show-Panel {
    [Windows.Forms.Application]::EnableVisualStyles()
    [Vyce.Monitor.PanelForm]::Prepare()
    $form = [Vyce.Monitor.PanelForm]::new($GamePid, $StdoutLog, $StderrLog)
    $timer = [Windows.Forms.Timer]::new()
    $game = @{ Process = $null; Pid = 0 }
    try {
        # Shown and drawn first: the sampler may have to compile its part.
        $form.Show()
        [Windows.Forms.Application]::DoEvents()
        try {
            . (Join-Path $PSScriptRoot 'vyce-monitor-resources.ps1')
            $timer.Interval = 1000
            $timer.Add_Tick({
                try {
                    if ($game.Pid -ne $form.GamePid) {
                        if ($game.Process) { $game.Process.Dispose() }
                        $game.Pid = $form.GamePid
                        $game.Process = Get-Process -Id $game.Pid -ErrorAction SilentlyContinue
                    }
                    $s = Get-VyceResourceSample -GameProcess $game.Process
                    $used = [double]::NaN
                    if ($null -ne $s.SystemRamTotalBytes -and $null -ne $s.SystemRamAvailableBytes) {
                        $used = ($s.SystemRamTotalBytes - $s.SystemRamAvailableBytes) / 1MB
                    }
                    $form.SetMachine(
                        (Get-Known $s.SystemCpuPercent), (Get-Known $s.GameCpuPercent), (Get-Known $s.GameThreads),
                        (Get-Known $s.LogicalProcessors), $used, (Get-Known $s.SystemRamTotalBytes) / 1MB,
                        (Get-Known $s.GameRamBytes) / 1MB)
                } catch {
                    [Vyce.Monitor.Faults]::Note('memória e processador', $_.Exception)
                }
            })
            $timer.Start()
        } catch {
            [Vyce.Monitor.Faults]::Note('memória e processador', $_.Exception)
        }
        [Windows.Forms.Application]::Run($form)
    } finally {
        $timer.Dispose()
        if ($game.Process) { $game.Process.Dispose() }
        $form.Dispose()
    }
}

# A plain window for where the panel itself does not open: the numbers of the
# last sample, the loading, and the warnings and errors as they are. `Why`
# is told at its foot.
function Show-PlainPanel([string]$Why) {
    $back = [Drawing.Color]::FromArgb(16, 20, 29)
    $ink = [Drawing.Color]::FromArgb(233, 240, 250)
    $form = [Windows.Forms.Form]::new()
    $form.Text = 'Vyce | Painel simples'
    $form.ClientSize = [Drawing.Size]::new(760, 560)
    $form.StartPosition = 'CenterScreen'
    if ($env:VYCE_PANEL_BOUNDS -match '^\s*(-?\d+),\s*(-?\d+),') {
        $form.StartPosition = 'Manual'
        $form.Location = [Drawing.Point]::new([int]$Matches[1], [int]$Matches[2])
    }
    $form.BackColor = $back
    $form.ForeColor = $ink
    $form.Font = [Drawing.Font]::new('Segoe UI', 10)
    $numbers = [Windows.Forms.Label]::new()
    $numbers.Dock = 'Top'
    $numbers.Height = 190
    $numbers.Padding = [Windows.Forms.Padding]::new(12)
    $numbers.Font = [Drawing.Font]::new('Segoe UI', 13)
    $numbers.Text = 'Lendo o log...'
    $list = [Windows.Forms.ListBox]::new()
    $list.Dock = 'Fill'
    $list.BackColor = $back
    $list.ForeColor = $ink
    $list.BorderStyle = 'None'
    $list.HorizontalScrollbar = $true
    $list.IntegralHeight = $false
    $note = [Windows.Forms.Label]::new()
    $note.Dock = 'Bottom'
    $note.Height = 48
    $note.Padding = [Windows.Forms.Padding]::new(12, 4, 12, 4)
    $note.Text = $Why
    $form.Controls.AddRange(@($list, $numbers, $note))
    $state = @{ At = -1L; Part = ''; Frame = $null; FrameAt = $null; Load = $null; Warnings = 0; Errors = 0; Decoder = [Text.Encoding]::UTF8.GetDecoder() }
    $game = Get-Process -Id $GamePid -ErrorAction SilentlyContinue
    $culture = [Globalization.CultureInfo]::GetCultureInfo('pt-BR')
    $say = { param($Value, $Format) if ($null -eq $Value) { '—' } else { ([double]$Value).ToString($Format, $culture) } }
    # "2026-11-26T04:45:56.672679Z  WARN victory_gpu::x: text" as "01:45:56  AVISO  text".
    $tidy = {
        param([string]$Line)
        if ($Line -match '^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(\.\d+)?Z\s+(WARN|ERROR)\s+[\w:]+:\s(.*)$') {
            $utc = [DateTime]::SpecifyKind([DateTime]::Parse($Matches[1], [Globalization.CultureInfo]::InvariantCulture), 'Utc')
            $word = if ($Matches[3] -eq 'WARN') { 'AVISO' } else { 'ERRO' }
            return $utc.ToLocalTime().ToString('HH:mm:ss') + "  $word  " + $Matches[4]
        }
        $Line
    }

    $read = {
        $file = $null
        try {
            $share = [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
            $file = [IO.File]::Open($StderrLog, [IO.FileMode]::Open, [IO.FileAccess]::Read, $share)
            # Opened late, only the end is read here.
            if ($state.At -lt 0 -or $file.Length -lt $state.At) { $state.At = [Math]::Max(0L, $file.Length - 2MB) }
            $count = [int][Math]::Min(1MB, $file.Length - $state.At)
            if ($count -le 0) { return }
            $file.Position = $state.At
            $bytes = [byte[]]::new($count)
            $got = $file.Read($bytes, 0, $count)
            $state.At += $got
            $chars = [char[]]::new($got + 2)
            $written = $state.Decoder.GetChars($bytes, 0, $got, $chars, 0)
            $lines = ($state.Part + [string]::new($chars, 0, $written)).Split([char]10)
            $state.Part = $lines[$lines.Length - 1]
            if ($state.Part.Length -gt 1MB) { $state.Part = '' }
            $frame = $null
            $load = $null
            $new = [Collections.Generic.List[string]]::new()
            for ($i = 0; $i -lt $lines.Length - 1; $i++) {
                $line = $lines[$i]
                if ($line.StartsWith('VYCE_TELEMETRY {', [StringComparison]::Ordinal)) { $frame = $line }
                elseif ($line.StartsWith('VYCE_LOADING {', [StringComparison]::Ordinal)) { $load = $line }
                elseif (-not $line.StartsWith('VYCE_', [StringComparison]::Ordinal)) {
                    $line = ($line -replace '\x1b\[[0-9;]*m', '').Trim()
                    if ($line -match '^(\S+\s+)?(ERROR|FATAL)\b|^VIctory: |panicked') { $state.Errors++; $new.Add((& $tidy $line)) }
                    elseif ($line -match '^(\S+\s+)?WARN\b') { $state.Warnings++; $new.Add((& $tidy $line)) }
                }
            }
            if ($frame) {
                try {
                    $state.Frame = ConvertFrom-Json -InputObject $frame.Substring(15)
                    $state.FrameAt = [DateTime]::Now
                } catch {
                    # A broken record.
                }
            }
            if ($load) {
                try { $state.Load = ConvertFrom-Json -InputObject $load.Substring(13) } catch {
                    # As above.
                }
            }
            if ($new.Count -gt 0) {
                $list.BeginUpdate()
                foreach ($line in ($new | Select-Object -Last 300)) {
                    if ($line.Length -gt 400) { $line = $line.Substring(0, 400) }
                    [void]$list.Items.Add($line)
                }
                while ($list.Items.Count -gt 300) { $list.Items.RemoveAt(0) }
                $list.TopIndex = $list.Items.Count - 1
                $list.EndUpdate()
            }
        } catch [IO.IOException] {
            # Not there yet, or being written.
        } finally {
            if ($file) { $file.Dispose() }
        }
    }
    $tell = {
        $f = $state.Frame
        $alive = $game -and -not $game.HasExited
        $text = @($(if ($alive) { 'Jogo em execução' } else { 'Jogo encerrado' }))
        if ($f) {
            $age = [int]([DateTime]::Now - $state.FrameAt).TotalSeconds
            $text += "$(& $say $f.fps '0.0') FPS, $(& $say $f.frame_ms '0.0') ms por quadro, há $age s"
            if ($alive -and $age -ge 4) { $text += "O jogo não apresentou quadros há $age s." }
            $text += "GPU $(& $say $f.gpu_wait_ms '0.0') ms, preparo $(& $say $f.cpu_prepare_ms '0.0') ms, cópia de volta $(& $say $f.copy_back_ms '0.0') ms"
            $text += "Desenhos $(& $say $f.draws_per_frame 'N0'), descartados $(& $say $f.dropped_per_frame 'N0'), pipelines na fila $(& $say $f.pipelines_building 'N0'), VRAM $(& $say $f.vram_mb 'N0') de $(& $say $f.vram_budget_mb 'N0') MB"
        } else { $text += 'Aguardando o primeiro quadro.' }
        if ($alive -and $state.Load -and $null -ne $state.Load.percent) {
            $text += "Carregando: $(& $say ([Math]::Floor([double]$state.Load.percent)) '0') %, faltam $(& $say $state.Load.left_s '0') s"
        }
        $text += "Avisos $($state.Warnings), erros $($state.Errors)"
        $numbers.Text = $text -join "`n"
    }
    $timer = [Windows.Forms.Timer]::new()
    $timer.Interval = 1000
    # One that fails leaves the other alone.
    $timer.Add_Tick({
        try { . $read } catch { $note.Text = "Leitura do log: $($_.Exception.Message)" }
        try { . $tell } catch { $note.Text = "Números: $($_.Exception.Message)" }
    })
    try {
        $timer.Start()
        [Windows.Forms.Application]::Run($form)
    } finally {
        $timer.Dispose()
        if ($game) { $game.Dispose() }
        $form.Dispose()
    }
}

$problem = $null
# VYCE_PANEL=simples opens the plain window, as a way around the panel.
if ($env:VYCE_PANEL -eq 'simples') {
    $problem = 'Painel simples, pedido por VYCE_PANEL=simples.'
} else {
    try {
        Import-Panel
        Show-Panel
    } catch {
        $problem = "O painel completo não abriu: $($_.Exception.Message)"
        [Console]::Error.WriteLine("painel: $_")
    }
}
if ($problem) { Show-PlainPanel $problem }
