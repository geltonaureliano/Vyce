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
. (Join-Path $PSScriptRoot 'vyce-monitor-resources.ps1')
[Windows.Forms.Application]::EnableVisualStyles()

$palette = @{
    Back = [Drawing.Color]::FromArgb(16, 20, 29)
    Panel = [Drawing.Color]::FromArgb(25, 31, 43)
    Log = [Drawing.Color]::FromArgb(13, 17, 24)
    Text = [Drawing.Color]::FromArgb(233, 240, 250)
    Muted = [Drawing.Color]::FromArgb(177, 189, 208)
    Accent = [Drawing.Color]::FromArgb(109, 222, 204)
    Warn = [Drawing.Color]::FromArgb(247, 205, 121)
    Error = [Drawing.Color]::FromArgb(255, 149, 156)
}

$form = [Windows.Forms.Form]::new()
$form.SuspendLayout()
$form.Text = 'Vyce | Diagnóstico'
$form.StartPosition = 'CenterScreen'
$form.AutoScaleMode = 'Font'
$form.ClientSize = [Drawing.Size]::new(970, 700)
$form.MinimumSize = [Drawing.Size]::new(850, 635)
$form.BackColor = $palette.Back
$form.ForeColor = $palette.Text
$form.Font = [Drawing.Font]::new('Segoe UI', 9)

$root = [Windows.Forms.TableLayoutPanel]::new()
$root.Dock = 'Fill'
$root.Padding = [Windows.Forms.Padding]::new(16, 12, 16, 12)
$root.ColumnCount = 1
$root.RowCount = 8
[void]$root.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 100))
for ($i = 0; $i -lt 6; $i++) {
    [void]$root.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
}
[void]$root.RowStyles.Add([Windows.Forms.RowStyle]::new('Percent', 100))
[void]$root.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
$form.Controls.Add($root)

function New-Label([string]$Text, [float]$Size, [Drawing.Color]$Color, [bool]$Bold = $false) {
    $label = [Windows.Forms.Label]::new()
    $label.Text = $Text
    $label.ForeColor = $Color
    $weight = if ($Bold) { [Drawing.FontStyle]::Bold } else { [Drawing.FontStyle]::Regular }
    $label.Font = [Drawing.Font]::new('Segoe UI', $Size, $weight)
    $label.Margin = [Windows.Forms.Padding]::new(3, 1, 3, 1)
    $label.AutoSize = $true
    $label.Dock = 'Fill'
    $label.TextAlign = 'MiddleLeft'
    $label.AutoEllipsis = $true
    $label
}

function New-MetricRow([string[]]$Names, [int]$Row, [int]$Size, [hashtable]$Details, [int[]]$Widths) {
    $line = [Windows.Forms.TableLayoutPanel]::new()
    $line.Dock = 'Fill'
    $line.AutoSize = $true
    $line.AutoSizeMode = 'GrowAndShrink'
    $line.ColumnCount = $Names.Count
    $line.RowCount = 1
    $line.Margin = [Windows.Forms.Padding]::new(0, 0, 0, 8)
    [void]$line.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
    $values = @{}
    $captions = @{}
    for ($i = 0; $i -lt $Names.Count; $i++) {
        $width = if ($Widths) { $Widths[$i] } else { 100 / $Names.Count }
        [void]$line.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', $width))
        $card = [Windows.Forms.TableLayoutPanel]::new()
        $card.Dock = 'Fill'
        $card.AutoSize = $true
        $card.AutoSizeMode = 'GrowAndShrink'
        $card.Padding = [Windows.Forms.Padding]::new(10, 7, 10, 7)
        $card.Margin = [Windows.Forms.Padding]::new(0, 0, $(if ($i -lt $Names.Count - 1) { 8 } else { 0 }), 0)
        $card.BackColor = $palette.Panel
        $card.ColumnCount = 1
        $card.RowCount = if ($Details.Count -gt 0) { 3 } else { 2 }
        [void]$card.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 100))
        for ($rowIndex = 0; $rowIndex -lt $card.RowCount; $rowIndex++) {
            [void]$card.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
        }
        $card.Controls.Add((New-Label $Names[$i] 9 $palette.Muted), 0, 0)
        $color = if ($i -eq 0 -and $Row -eq 1) { $palette.Accent } else { $palette.Text }
        $value = New-Label '—' $Size $color $true
        $card.Controls.Add($value, 0, 1)
        if ($Details.ContainsKey($Names[$i])) {
            $caption = New-Label $Details[$Names[$i]] 8 $palette.Muted
            $card.Controls.Add($caption, 0, 2)
            $captions[$Names[$i]] = $caption
        }
        $line.Controls.Add($card, $i, 0)
        $values[$Names[$i]] = $value
    }
    $root.Controls.Add($line, 0, $Row)
    @{ Values = $values; Details = $captions }
}

$header = [Windows.Forms.TableLayoutPanel]::new()
$header.Dock = 'Fill'
$header.AutoSize = $true
$header.AutoSizeMode = 'GrowAndShrink'
$header.Margin = [Windows.Forms.Padding]::new(0, 0, 0, 9)
$header.ColumnCount = 2
$header.RowCount = 2
[void]$header.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 63))
[void]$header.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 37))
[void]$header.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
[void]$header.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
$header.Controls.Add((New-Label 'Diagnóstico do Vyce' 22 $palette.Text $true), 0, 0)
$header.Controls.Add((New-Label "Sessão do jogo  ·  PID $GamePid" 9 $palette.Muted), 0, 1)
$statusLabel = New-Label 'Conectando ao jogo' 11 $palette.Accent $true
$statusLabel.TextAlign = 'MiddleRight'
$header.Controls.Add($statusLabel, 1, 0)
$durationLabel = New-Label '' 9 $palette.Muted
$durationLabel.TextAlign = 'MiddleRight'
$header.Controls.Add($durationLabel, 1, 1)
$root.Controls.Add($header, 0, 0)

$mainMetrics = (New-MetricRow @('FPS', 'Tempo por quadro', 'Draw calls / quadro', 'Quadros apresentados') 1 21 @{}).Values
$work = New-MetricRow @('Preparo na CPU', 'Espera pela GPU', 'Cópia de volta', 'Descartes / quadro', 'Pipelines em criação') 2 15 @{
    'Preparo na CPU' = 'Snapshots —'
    'Espera pela GPU' = 'GPU ocupada —'
    'Cópia de volta' = 'Assíncronas —'
    'Pipelines em criação' = 'Espera CPU —'
}
$workMetrics = $work.Values
$resources = New-MetricRow @('RAM do sistema', 'RAM do jogo', 'CPU do sistema', 'CPU do jogo', 'Threads do jogo') 3 14 @{
    'RAM do sistema' = '— disponíveis'
    'RAM do jogo' = 'Memória residente'
    'CPU do sistema' = '— processadores lógicos'
    'CPU do jogo' = 'Da capacidade total'
} @(25, 19, 21, 18, 17)

$activity = [Windows.Forms.TableLayoutPanel]::new()
$activity.Dock = 'Fill'
$activity.AutoSize = $true
$activity.AutoSizeMode = 'GrowAndShrink'
$activity.BackColor = $palette.Panel
$activity.Margin = [Windows.Forms.Padding]::new(0, 0, 0, 6)
$activity.Padding = [Windows.Forms.Padding]::new(10, 5, 10, 5)
$activity.ColumnCount = 1
$activity.RowCount = 2
[void]$activity.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 100))
[void]$activity.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
[void]$activity.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
$activityLabel = New-Label 'Aguardando a primeira amostra de renderização' 10 $palette.Text $true
$activity.Controls.Add($activityLabel, 0, 0)
$activityDetail = New-Label 'Acompanhe o log abaixo enquanto o jogo inicia.' 9 $palette.Muted
$activity.Controls.Add($activityDetail, 0, 1)
$root.Controls.Add($activity, 0, 4)

$logBar = [Windows.Forms.TableLayoutPanel]::new()
$logBar.Dock = 'Fill'
$logBar.AutoSize = $true
$logBar.AutoSizeMode = 'GrowAndShrink'
$logBar.Margin = [Windows.Forms.Padding]::new(0, 0, 0, 4)
$logBar.ColumnCount = 3
$logBar.RowCount = 1
[void]$logBar.RowStyles.Add([Windows.Forms.RowStyle]::new('AutoSize'))
[void]$logBar.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('Percent', 100))
[void]$logBar.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('AutoSize'))
[void]$logBar.ColumnStyles.Add([Windows.Forms.ColumnStyle]::new('AutoSize'))
$logBar.Controls.Add((New-Label 'Logs recentes' 12 $palette.Text $true), 0, 0)
$follow = [Windows.Forms.CheckBox]::new()
$follow.Text = 'Seguir logs'
$follow.ForeColor = $palette.Muted
$follow.CheckAlign = 'MiddleLeft'
$follow.Checked = $true
$follow.AutoSize = $true
$follow.Dock = 'Fill'
$follow.Margin = [Windows.Forms.Padding]::new(0, 0, 14, 0)
$logBar.Controls.Add($follow, 1, 0)
$openLogs = [Windows.Forms.Button]::new()
$openLogs.Text = 'Abrir pasta'
$openLogs.FlatStyle = 'Flat'
$openLogs.FlatAppearance.BorderColor = $palette.Muted
$openLogs.ForeColor = $palette.Text
$openLogs.BackColor = $palette.Panel
$openLogs.Dock = 'Fill'
$openLogs.AutoSize = $true
$openLogs.AutoSizeMode = 'GrowAndShrink'
$openLogs.Padding = [Windows.Forms.Padding]::new(9, 1, 9, 1)
$openLogs.Margin = [Windows.Forms.Padding]::new(0)
$openLogs.Add_Click({
    $folder = [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($StderrLog))
    if (Test-Path -LiteralPath $folder -PathType Container) {
        Start-Process -FilePath explorer.exe -ArgumentList ('"' + $folder + '"')
    }
})
$logBar.Controls.Add($openLogs, 2, 0)
$root.Controls.Add($logBar, 0, 5)

$logView = [Windows.Forms.RichTextBox]::new()
$logView.Dock = 'Fill'
$logView.Margin = [Windows.Forms.Padding]::new(0)
$logView.BorderStyle = 'None'
$logView.BackColor = $palette.Log
$logView.ForeColor = $palette.Text
$logView.Font = [Drawing.Font]::new('Consolas', 9)
$logView.ReadOnly = $true
$logView.WordWrap = $false
$logView.DetectUrls = $false
$logView.ScrollBars = 'Both'
$root.Controls.Add($logView, 0, 6)
$footer = New-Label 'As métricas do jogo aparecem quando o renderizador emitir uma amostra.' 9 $palette.Muted
$footer.Margin = [Windows.Forms.Padding]::new(3, 7, 3, 0)
$root.Controls.Add($footer, 0, 7)
$form.ResumeLayout($true)

function New-LogTail([string]$Path) {
    @{
        Path = $Path
        Position = 0L
        Pending = ''
        SkipFragment = $false
        Decoder = [Text.Encoding]::UTF8.GetDecoder()
    }
}

function Read-LogTail([hashtable]$Tail) {
    $handle = $null
    try {
        $share = [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
        $handle = [IO.File]::Open($Tail.Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, $share)
        if ($handle.Length -lt $Tail.Position) {
            $Tail.Position = 0L
            $Tail.Pending = ''
            $Tail.SkipFragment = $false
            $Tail.Decoder.Reset()
        }
        # Catch up once to the recent tail if the monitor opened late.
        if ($Tail.Position -eq 0 -and $handle.Length -gt 131072) {
            $Tail.Position = $handle.Length - 131072
            $Tail.SkipFragment = $true
        }
        $count = [int][Math]::Min(32768, $handle.Length - $Tail.Position)
        if ($count -le 0) { return }
        $handle.Position = $Tail.Position
        $bytes = [byte[]]::new($count)
        $read = $handle.Read($bytes, 0, $count)
        if ($read -le 0) { return }
        $Tail.Position += $read
        $characters = [char[]]::new($read + 2)
        $written = $Tail.Decoder.GetChars($bytes, 0, $read, $characters, 0)
        $chunk = $Tail.Pending + [string]::new($characters, 0, $written)
        $lines = $chunk.Split([char]10)
        $Tail.Pending = $lines[$lines.Length - 1]
        $skipFirst = $Tail.SkipFragment
        if ($lines.Length -gt 1) { $Tail.SkipFragment = $false }
        if ($Tail.Pending.Length -gt 8192) {
            $Tail.Pending = ''
            $Tail.SkipFragment = $true
        }
        $start = if ($skipFirst) { 1 } else { 0 }
        for ($i = $start; $i -lt $lines.Length - 1; $i++) {
            $lines[$i].TrimEnd([char]13).TrimStart([char]0xFEFF)
        }
    } catch [IO.IOException] {
        # Startup and concurrent log rotation can momentarily make a file unavailable.
    } finally {
        if ($null -ne $handle) { $handle.Dispose() }
    }
}

$monitor = @{
    LastFrame = $null
    LastUpdate = $null
    LastLog = $null
    Stage = $null
    Warnings = 0
    Errors = 0
    Lengths = [Collections.Generic.Queue[int]]::new()
}
$tails = @(
    (New-LogTail $StdoutLog)
    (New-LogTail $StderrLog)
)
$gameProcess = Get-Process -Id $GamePid -ErrorAction SilentlyContinue
$sessionStart = Get-Date
if ($gameProcess) {
    try { $sessionStart = $gameProcess.StartTime } catch {
        # The game can exit before the monitor has opened.
    }
}
$sessionEnd = $null
$displayCulture = [Globalization.CultureInfo]::GetCultureInfo('pt-BR')

function Format-Metric($Value, [string]$Format, [string]$Suffix = '') {
    if ($null -eq $Value -or $Value -isnot [ValueType]) { return '—' }
    $numeric = [double]$Value
    if ([double]::IsNaN($numeric) -or [double]::IsInfinity($numeric) -or $numeric -lt 0) { return '—' }
    $numeric.ToString($Format, $displayCulture) + $Suffix
}

function Update-ResourceCards {
    $sample = Get-VyceResourceSample -GameProcess $gameProcess
    $values = $resources.Values
    $details = $resources.Details
    if ($sample.SystemRamTotalBytes -gt 0 -and $null -ne $sample.SystemRamAvailableBytes) {
        $used = $sample.SystemRamTotalBytes - $sample.SystemRamAvailableBytes
        $usedGiB = Format-Metric ($used / 1GB) '0.0'
        $totalGiB = Format-Metric ($sample.SystemRamTotalBytes / 1GB) '0.0' ' GiB'
        $values['RAM do sistema'].Text = "$usedGiB / $totalGiB"
        $details['RAM do sistema'].Text = Format-Metric ($sample.SystemRamAvailableBytes / 1GB) '0.0' ' GiB disponíveis'
    } else {
        $values['RAM do sistema'].Text = '—'
        $details['RAM do sistema'].Text = 'Disponível: —'
    }

    $values['RAM do jogo'].Text = if ($null -eq $sample.GameRamBytes) {
        '—'
    } elseif ($sample.GameRamBytes -ge 1GB) {
        Format-Metric ($sample.GameRamBytes / 1GB) '0.0' ' GiB'
    } else {
        Format-Metric ($sample.GameRamBytes / 1MB) 'N0' ' MiB'
    }
    $values['CPU do sistema'].Text = Format-Metric $sample.SystemCpuPercent '0.0' '%'
    $values['CPU do jogo'].Text = Format-Metric $sample.GameCpuPercent '0.0' '%'
    $values['Threads do jogo'].Text = Format-Metric $sample.GameThreads 'N0'
    $details['CPU do sistema'].Text = if ($null -ne $sample.LogicalProcessors) {
        "$($sample.LogicalProcessors) CPUs lógicas"
    } else {
        'CPUs lógicas: —'
    }
}

function Add-LogLine([string]$Line, [bool]$FromStderr) {
    if ($Line.StartsWith('VYCE_TELEMETRY ', [StringComparison]::Ordinal)) {
        try {
            $item = ConvertFrom-Json -InputObject $Line.Substring(15) -ErrorAction Stop
            if ($item.schema -eq 1 -and $item.event -eq 'frame') {
                $monitor.LastFrame = $item
                $monitor.LastUpdate = [DateTime]::Now
            }
        } catch {
            # An incomplete or unknown telemetry record should not stop the monitor.
        }
        return
    }
    if ([string]::IsNullOrWhiteSpace($Line)) { return }
    $monitor.LastLog = [DateTime]::Now
    if ($Line -match '^build\s{2,}') {
        $monitor.Stage = 'Build do jogo identificada'
    } elseif ($Line -match '^mapped at\s+') {
        $monitor.Stage = 'Executável mapeado'
    } elseif ($Line -match '^bound\s+game_scarlett_master\.exe\b.*\b0 missing\b') {
        $monitor.Stage = 'Vínculos do executável resolvidos'
    }
    $level = [regex]::Match(
        $Line, '^\s*(?:\d{4}-\S+\s+)?\[?(ERROR|FATAL|PANIC|WARN(?:ING)?)\]?(?=[:\s]|$)',
        [Text.RegularExpressions.RegexOptions]::IgnoreCase
    ).Groups[1].Value
    $color = $palette.Text
    if ($level -match '^(ERROR|FATAL|PANIC)$') {
        $monitor.Errors++
        $color = $palette.Error
    } elseif ($level) {
        $monitor.Warnings++
        $color = $palette.Warn
    } elseif ($FromStderr) {
        $color = $palette.Muted
    }
    # The full original line stays in the session log on disk.
    if ($Line.Length -gt 760) { $Line = $Line.Substring(0, 757) + '...' }
    $size = $logView.TextLength
    $logView.SelectionStart = $size
    $logView.SelectionLength = 0
    $logView.SelectionColor = $color
    $logView.AppendText($Line + [char]10)
    $monitor.Lengths.Enqueue($logView.TextLength - $size)
}

function Update-Monitor {
    $caret = $logView.SelectionStart
    for ($i = 0; $i -lt $tails.Count; $i++) {
        foreach ($line in (Read-LogTail $tails[$i])) { Add-LogLine $line ($i -eq 1) }
    }
    $remove = 0
    while ($monitor.Lengths.Count -gt 180 -or ($logView.TextLength - $remove) -gt 48000) {
        $remove += $monitor.Lengths.Dequeue()
    }
    if ($remove -gt 0) {
        $logView.Select(0, [Math]::Min($remove, $logView.TextLength))
        $logView.SelectedText = ''
    }
    if ($follow.Checked) {
        $logView.SelectionStart = $logView.TextLength
        $logView.ScrollToCaret()
    } else {
        $logView.SelectionStart = [Math]::Max(0, [Math]::Min($logView.TextLength, $caret - $remove))
    }

    $alive = $null -ne $gameProcess -and -not $gameProcess.HasExited
    $now = [DateTime]::Now
    if ($alive) {
        $statusLabel.Text = '●  Jogo em execução'
        $statusLabel.ForeColor = $palette.Accent
    } else {
        if ($null -eq $script:sessionEnd) { $script:sessionEnd = $now }
        $statusLabel.Text = '○  Jogo encerrado'
        $statusLabel.ForeColor = $palette.Muted
    }
    $end = if ($alive) { $now } else { $script:sessionEnd }
    $span = $end - $sessionStart
    $length = '{0:00}:{1:00}:{2:00}' -f [Math]::Floor($span.TotalHours), $span.Minutes, $span.Seconds
    $durationLabel.Text = "Duração  $length"
    Update-ResourceCards

    $frame = $monitor.LastFrame
    $mainMetrics['FPS'].Text = Format-Metric $frame.fps '0.0'
    $mainMetrics['Tempo por quadro'].Text = Format-Metric $frame.frame_ms '0.0' ' ms'
    $mainMetrics['Draw calls / quadro'].Text = Format-Metric $frame.draws_per_frame 'N0'
    $mainMetrics['Quadros apresentados'].Text = Format-Metric $frame.frames_total 'N0'
    $workMetrics['Preparo na CPU'].Text = Format-Metric $frame.cpu_prepare_ms '0.0' ' ms'
    $workMetrics['Espera pela GPU'].Text = Format-Metric $frame.gpu_wait_ms '0.0' ' ms'
    $workMetrics['Cópia de volta'].Text = Format-Metric $frame.copy_back_ms '0.0' ' ms'
    $workMetrics['Descartes / quadro'].Text = Format-Metric $frame.dropped_per_frame '0.0'
    $workMetrics['Pipelines em criação'].Text = Format-Metric $frame.pipelines_building 'N0'
    $pipelineWait = Format-Metric $frame.pipeline_wait_ms '0.0' ' ms/quadro'
    $work.Details['Pipelines em criação'].Text = "Espera CPU $pipelineWait"
    # Absent in the telemetry of older builds, where these show a dash.
    $work.Details['Preparo na CPU'].Text = 'Snapshots ' + (Format-Metric $frame.snapshot_ms '0.0' ' ms')
    $work.Details['Espera pela GPU'].Text = 'GPU ocupada ' + (Format-Metric $frame.gpu_busy_ms '0.0' ' ms')
    $work.Details['Cópia de volta'].Text = 'Assíncronas ' + (Format-Metric $frame.unwaited_per_frame '0.0') +
        ' de ' + (Format-Metric $frame.submissions_per_frame '0.0')

    if (!$alive) {
        $activityLabel.Text = 'Sessão encerrada'
        $activityDetail.Text = 'Os últimos dados e logs continuam disponíveis aqui.'
    } elseif ($null -ne $frame) {
        $seconds = [int][Math]::Max(0, ($now - $monitor.LastUpdate).TotalSeconds)
        if ($seconds -ge 4) {
            $activityLabel.Text = "Sem nova amostra de quadro há $seconds s"
        } elseif ($frame.pipelines_building -gt 0) {
            $activityLabel.Text = 'Renderizando e preparando pipelines'
        } else {
            $activityLabel.Text = 'O renderizador está apresentando quadros'
        }
        $activityDetail.Text = 'Amostras podem vir de menus, do carregamento ou do jogo.'
    } else {
        $activityLabel.Text = 'Aguardando a primeira amostra de renderização'
        $activityDetail.Text = if ($monitor.Stage) { $monitor.Stage } else { 'Acompanhe o log abaixo enquanto o jogo inicia.' }
    }
    $counts = "Avisos $($monitor.Warnings)  ·  Erros $($monitor.Errors)"
    if ($monitor.LastLog) {
        $counts += "  ·  Última linha lida às $($monitor.LastLog.ToString('HH:mm:ss'))"
    }
    if ($null -ne $frame.vram_mb) {
        $counts += "  ·  VRAM $(Format-Metric $frame.vram_mb 'N0') de $(Format-Metric $frame.vram_budget_mb 'N0' ' MB')"
    }
    $footer.Text = $counts
}

$timer = [Windows.Forms.Timer]::new()
$timer.Interval = 1000
function Read-MonitorTick {
    try { Update-Monitor } catch {
        $footer.Text = "Leitura temporariamente indisponível: $($_.Exception.Message)"
    }
}
$timer.Add_Tick({ Read-MonitorTick })
$form.Add_Shown({ Read-MonitorTick; $timer.Start() })
try {
    [void]$form.ShowDialog()
} finally {
    $timer.Stop()
    $timer.Dispose()
    if ($gameProcess) { $gameProcess.Dispose() }
    $form.Dispose()
}
