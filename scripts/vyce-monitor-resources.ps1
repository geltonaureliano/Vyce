# Windows system counters for the monitor. No WMI or localized counter names.
if (-not ('Vyce.Monitor.NativeResources' -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

namespace Vyce.Monitor
{
    public static class NativeResources
    {
        [StructLayout(LayoutKind.Sequential)]
        private struct FileTime
        {
            public uint Low;
            public uint High;

            public ulong Ticks { get { return ((ulong)High << 32) | Low; } }
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct MemoryStatus
        {
            public uint Length;
            public uint MemoryLoad;
            public ulong TotalPhys;
            public ulong AvailPhys;
            public ulong TotalPageFile;
            public ulong AvailPageFile;
            public ulong TotalVirtual;
            public ulong AvailVirtual;
            public ulong AvailExtendedVirtual;
        }

        public struct Counters
        {
            public bool HasCpu;
            public ulong Idle;
            public ulong Kernel;
            public ulong User;
            public bool HasRam;
            public ulong RamTotal;
            public ulong RamAvailable;
        }

        private static readonly uint MemoryStatusSize = (uint)Marshal.SizeOf(typeof(MemoryStatus));

        [DllImport("kernel32.dll", ExactSpelling = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetSystemTimes(out FileTime idle, out FileTime kernel, out FileTime user);

        [DllImport("kernel32.dll", ExactSpelling = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GlobalMemoryStatusEx(ref MemoryStatus status);

        [DllImport("kernel32.dll", ExactSpelling = true)]
        private static extern uint GetActiveProcessorCount(ushort groupNumber);

        public static int LogicalProcessors()
        {
            uint count = GetActiveProcessorCount(0xffff);
            return count == 0 ? Environment.ProcessorCount : (int)count;
        }

        public static Counters Read()
        {
            FileTime idle, kernel, user;
            var memory = new MemoryStatus { Length = MemoryStatusSize };
            var result = new Counters();
            result.HasCpu = GetSystemTimes(out idle, out kernel, out user);
            result.Idle = idle.Ticks;
            result.Kernel = kernel.Ticks;
            result.User = user.Ticks;
            result.HasRam = GlobalMemoryStatusEx(ref memory);
            result.RamTotal = memory.TotalPhys;
            result.RamAvailable = memory.AvailPhys;
            return result;
        }
    }
}
"@
}

$script:VyceResourceState = @{
    System = $null
    Game = $null
    LogicalProcessors = [Vyce.Monitor.NativeResources]::LogicalProcessors()
}

function Get-VyceResourceSample {
    [CmdletBinding()]
    param(
        [AllowNull()]
        [System.Diagnostics.Process]$GameProcess
    )

    $state = $script:VyceResourceState
    $now = [Vyce.Monitor.NativeResources]::Read()
    $systemCpu = $null
    # GetSystemTimes measures only the caller's processor group on >64-CPU hosts.
    if ($now.HasCpu -and $state.LogicalProcessors -le 64) {
        $before = $state.System
        if ($null -ne $before) {
            $idle = [double]$now.Idle - [double]$before.Idle
            $kernel = [double]$now.Kernel - [double]$before.Kernel
            $user = [double]$now.User - [double]$before.User
            $total = $kernel + $user
            # Windows includes the idle time in its kernel time total.
            if ($idle -ge 0 -and $kernel -ge 0 -and $user -ge 0 -and $total -gt 0) {
                $systemCpu = [Math]::Max(0.0, [Math]::Min(100.0, 100.0 * ($total - $idle) / $total))
            }
        }
        $state.System = $now
    } else {
        $state.System = $null
    }

    $gameCpu = $null
    $gameRam = $null
    $gameThreads = $null
    $game = $null
    if ($null -ne $GameProcess) {
        try {
            $GameProcess.Refresh()
            $processId = $GameProcess.Id
            if ($processId -gt 0 -and $GameProcess.HasExited -eq $false) {
                $started = $GameProcess.StartTime
                $ram = $GameProcess.WorkingSet64
                $threads = $GameProcess.Threads.Count
                $cpu = $GameProcess.TotalProcessorTime
                $timestamp = [System.Diagnostics.Stopwatch]::GetTimestamp()
                if ($null -ne $started -and $null -ne $ram -and $threads -gt 0 -and $null -ne $cpu) {
                    $gameRam = $ram
                    $gameThreads = $threads
                    $game = @{
                        Id = $processId
                        Started = $started.Ticks
                        CpuTicks = $cpu.Ticks
                        Timestamp = $timestamp
                    }
                }
            }
        } catch {
            # The process may exit between refreshing and reading its counters.
            $gameRam = $null
            $gameThreads = $null
        }
    }

    $previous = $state.Game
    if ($null -ne $game -and $null -ne $previous -and $game.Id -eq $previous.Id -and $game.Started -eq $previous.Started) {
        $interval = ($game.Timestamp - $previous.Timestamp) / [System.Diagnostics.Stopwatch]::Frequency
        $cpuTime = ($game.CpuTicks - $previous.CpuTicks) / [TimeSpan]::TicksPerSecond
        if ($interval -gt 0 -and $cpuTime -ge 0) {
            $capacity = $interval * $state.LogicalProcessors
            $gameCpu = [Math]::Max(0.0, [Math]::Min(100.0, 100.0 * $cpuTime / $capacity))
        }
    }
    $state.Game = $game

    [pscustomobject]@{
        GameCpuPercent = $gameCpu
        SystemCpuPercent = $systemCpu
        LogicalProcessors = $state.LogicalProcessors
        GameRamBytes = $gameRam
        SystemRamTotalBytes = if ($now.HasRam) { $now.RamTotal } else { $null }
        SystemRamAvailableBytes = if ($now.HasRam) { $now.RamAvailable } else { $null }
        GameThreads = $gameThreads
    }
}
