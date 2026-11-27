# Console wave64 compute on 32-lane hosts

Status: steps 1, 2 and 4 to 6 are in. On NVIDIA, wave64 programs whose lanes
read only their own half run as two 32-lane waves. Groups of one console
wave also run in the console flow: masks read whole, lanes of the other
half, divergent branches, loops and single-wave LDS. Groups of several
console waves run in step where their waves branch alike, and in paired
lanes (one host wave a console wave, two console lanes a thread) where
nothing else takes them. Still open: `ds_permute_b32` scatter, and in
paired lanes the lane operations outside whole-wave sections that version
0 leaves out.

## The problem

The console runs most of the title's compute programs in waves of 64
lanes. Per-lane code does not care how many lanes a host wave has, so most
of them translate without a wave size. A program that depends on the lane
layout declares the console's (`WaveSize(64)`, see `emit::lower`, where
`wave_size` is set):

- **console lanes**: it reads other lanes (`v_readlane`, `v_writelane`,
  DPP, `ds_swizzle`, `ds_(b)permute`, `v_permlane*`), counts with
  `v_mbcnt`, reads a lane mask as a number, or tests its own bit of a mask
  held as a number;
- **one wave sharing LDS**: the group is a single console wave and uses
  LDS, where the console compiler leaves out barriers because one wave runs
  in order.

D3D12 reports NVIDIA's wave range as 32..32, so these programs cannot run
there as they are. Since step 1 they stop at translation with the reason
(`emit::Target`) instead of being refused at pipeline creation.
`victory translate --host-lanes 32-32` counts them.

## The form

One console wave of 64 runs as two host waves of 32 lanes ("halves") of the
same thread group. The shader declares `WaveSize(32)`; console lane
= console thread % 64, half = bit 5 of it.

**Lane assignment.** The relabelling in `Lower::thread_id` assumes each
host wave holds a wave-aligned run of flat thread ids: it takes the first
lane's flat id, rounded down to the declared wave size, plus the host lane.
XCL's notes report NVIDIA forming waves from non-consecutive thread indices
in some 3-D groups. On an RTX 5070 with this relabelling, every group
`emit::split_tests` tries is exact: 32, 64 and 128 threads in 1-D, and
4x4x4, 8x8x2, 2x4x8, 16x2x4, 8x8x4 and 4x8x8. If another driver breaks it,
the fallback is to take console threads from a groupshared counter: one lane
of each host wave adds 32 to it when the wave starts and shares the result,
and the wave's lanes take the next 32 console threads in lane order.

**What stays inside a half** needs nothing: DPP16 and DPP8 (rows of 16,
groups of 8), `ds_swizzle` (groups of 32 or 4), and `ds_bpermute_b32`
(gather within 32 lanes). `v_permlane16` and `v_permlanex16` also stay
within a half, but their lowering does not yet support the split form.
The split DPP and `ds_swizzle` paths use the same source-lane calculation
as on a native Wave64 host.

RDNA 2 takes the gather index from bits 6:2 of the byte address. A
gather in the upper half maps that index to the upper half on a native
Wave64 host. The DXIL lowering checks the ballot before reading the lane,
so an EXEC-disabled or absent source gives zero on either host.

Step 4 (`split` in `Lower::new`) runs a program as two 32-lane waves when
the host has no 64-lane waves and the program:
- reads other lanes only through DPP, `ds_swizzle` or `ds_bpermute_b32`;
- reads no mask whole and tests no bit of a mask held as a number;
- has no whole-wave sections (their reductions would cover a half);
- uses no LDS (the console compiler may have left out barriers between
  lanes of one wave).

Anything else still stops with the step 1 reason. `mask_dword`, the wave
reduction lowering and `lane_bit` refuse the split form as a safety net.
`v_readfirstlane` reads the first active lane of the host wave, as it
already did for every wave64 program without a declared wave size.

**What crosses halves** goes through an exchange in groupshared memory:

| operation | exchanged |
|---|---|
| a lane mask read whole (ballots: scalar compares, `s_bcnt1`, `s_ff1`, mask tests) | each half's 32-bit ballot |
| `v_mbcnt_lo` in the upper half | the lower half's ballot |
| `v_readlane`, `v_readfirstlane` (uniform lane) | the value of lane `l & 31` from the half that owns `l` |
| `v_writelane` to the other half | the value and lane |

A mask read only lane by lane (a `v_cndmask` selector, a carry-in) needs
just the lane's own half. Exchanging those anyway is correct but slow.

## Synchronisation: the console flow

Both halves must reach every exchange together. On the console they do,
because a wave runs every instruction whatever EXEC holds. Our per-lane
structurisation turns EXEC into per-lane branches, so a half with no lane
on a path would skip that path and never reach an exchange there.

An earlier version of this plan made only the regions with an exchange
uniform. It turned out cleaner to run the whole program as the console
does (step 5, `lower::CONSOLE_FLOW`):

- `structure::structure_console` builds the per-lane booleans without
  resolving mask joins from what arriving lanes know, since every lane
  arrives. Each join that differs between edges becomes a phi.
  `Structured::execs` gives the lane's EXEC bit at each vector
  instruction.
- The lowering guards every vector instruction with that EXEC bit.
  `v_readlane`, `v_writelane` and `v_readfirstlane` ignore EXEC, as on the
  console.
- Branches are the console's `s_cbranch_*`. EXEC and VCC tests, and the
  SCC of a mask operation, ask whether some lane of the console wave has
  the bit: each half's ballot, exchanged. Both halves then take every
  branch the same way, and loops run while any lane of the console wave
  does.
- Exchange (`Lower::exchange`): each half stores its dword to group-shared
  slot `half`, then a group barrier, both read both slots, and a second
  barrier. Every thread of the group reaches it because the group is one
  console wave whose halves take the same branches. A group of 32 threads
  or fewer has no upper half: its word is 0 and no barrier is needed.
- Masks read whole (`mask_dword`) are the exchanged ballots. Lane bits of
  masks held as numbers use the console lane. `v_readlane` and
  `v_readfirstlane` read the owning half's value. `v_writelane` and
  `v_mbcnt` count with the console lane.

`lower` tries the per-lane form first. A program that needs the console
flow stops with `CONSOLE_FLOW`, and `emit` lowers it again from the
console structure. Eligible: wave64 compute on a 32-lane host, at most
64 threads per group, no `ds_permute_b32` and no whole-wave sections.

For a single wave using LDS, the lowering emits a group barrier after each
LDS instruction, after both halves have rejoined from any EXEC guard.
Reads with an inactive EXEC lane skip the memory access. The group's LDS
and the two exchange dwords must fit in 32 KiB. Address-based dispatches
that can let individual threads return early are ineligible for this LDS
path. `emit::split_tests` checks a read across halves inside a masked
branch against known output values.

Groups of several console waves run the console flow in step
(`emit::lockstep`) when every wave of the group takes the same way at each
branch: branches on EXEC (`s_cbranch_execz`/`execnz`) and on SCC from
scalars the whole group shares. Each console wave then has its own pair of
exchange dwords, and all waves of the group meet at the same barriers.
Programs whose waves may branch apart keep another form or are refused
(`waves_that_branch_apart_do_not_run_in_step`).

Programs whose lanes only meet within their own 32 (each host wave a wave
of its own: wave sums and prefix sums, as fxceless/victory-mix found) run as
local waves (`emit::local_waves`), tried where the per-lane form is
refused.

A group of one console wave that uses LDS runs as local waves too when its
wave operations are only the allocation idiom (`VICTORY_LDS_LOCAL=0` turns
it off): the console compiler left out the barriers between the wave's LDS
accesses, so the two host waves meet (a group barrier) before each LDS
access in a block every lane runs, and before and after each lane-branch
region with LDS accesses (`lower::lds_meetings`, after VIctory-7900XTX). A
region with both an LDS read and an LDS write cannot be ordered so and keeps
the console flow. The culls (CS_Tree_BatchCull) reach it only without their
early-out, which keeps the body out of the blocks every lane runs. On the
RTX 2070 SUPER the shared-LDS and idiom patterns give the console flow's
results (`split_tests::local_waves_of_one_wave_groups_keep_their_lds_order`),
the idiom's slots in another order.

A group barrier must be met as often by every thread, or the threads that
reach it wait for good (a hang, then a reset of the card). The blocks where
the waves meet are therefore the ones both host waves run as often, which
the fork's rule did not quite give (`VICTORY_LOCAL_MEET_CHECKS=0` brings it
back):

- A loop that lanes leave after different numbers of turns is a region of
  its own, its exit test included: a lane branch's region never held the
  branch's block, so the latch of such a loop met the other host wave once
  a turn, a different number of times in each thread. The waves now meet
  before the loop and after it.
- A uniform branch can still part the two host waves: each reserves slots of
  its own with the first-lane atomic of the allocation idiom, and a lane
  read (`v_readfirstlane_b32`, `v_readlane_b32`) of a value the lanes hold
  apart reads a lane of its own host wave. Such scalars, what is made of
  them, lane masks read as numbers and what a lane branch's region leaves
  behind (`local_waves::host_wave_varying`) make a branch on them a region
  too. A value counts as held alike in the lanes only if everything its
  instruction reads is (`VICTORY_HOST_WAVE_READS=0` brings back 1952aaf's
  rule, which looked at the operands and a partial write only): beside the
  operands, the old value of a destination it accumulates into
  (`v_fmac_f32`, `v_mac_f32`, `v_fmac_legacy_f32`, `v_dot4c_i32_i8`, the
  16-bit and packed forms), swaps or writes in part, any other VGPR, and
  `v_div_fmas_f32`'s VCC; only the old value under a whole write is not
  read. A lane mask read lane by lane (a select's, a carry-in, that VCC)
  must hold one bit in every lane: a compare or carry of alike values that
  no region left behind, no lane or every lane, or a uniform select (every
  lane's bit an SCC); a constant of some lanes, or one loaded or built from
  scalars, does not.
- So does a scalar load of memory the dispatch itself may write while it
  runs (`local_waves::loads_of_written_memory`): every one with `glc` or
  `dlc`, and in a program that stores to memory or adds to it, every one
  of a view its resources do not name a constant buffer or shader resource
  view (an unordered-access view, a heap descriptor, a root view of no
  given kind). Another group, or the other host wave, can land a write
  between the two host waves' loads, which on the console are one load.
  A constant buffer or shader resource view is taken not to be written
  while a dispatch reads it, through another view either (D3D12 keeps a
  resource in a read state apart from unordered access); a table's
  descriptors and the code's constants never are.
- A program that reaches memory by address returns at its entry in the
  threads of the last group past the dispatch, each alone: no meetings
  (the shared-wave and lockstep forms knew it).

The translation of these cases (a loop, a branch on the reserved base, on a
lane read of an accumulator or of a constant mask's select, on a flag
loaded with `glc` or through the view the program stores to) is checked on
the module (no barrier between a loop's header and its back edge, as many
barriers on every way to the end; 1952aaf's analysis met inside the last
four branches), then run on the card, against the console flow where it
takes them (it has no lane read of a constant mask's select)
(`split_tests::local_waves_meet_only_where_both_host_waves_run_as_often`,
`local_waves_of_loops_and_reserved_branches_match_the_console`). What is
alike and what is apart, accumulators, swaps, carries, divisions after a
loop and the kinds of loaded views included, is checked case by case
(`host_waves_hold_lane_reads_and_reserved_slots_apart`,
`host_waves_may_load_written_memory_apart`).

### Forms without early-outs or skips

A wave64 compute program that the 32-lane forms refuse is tried again
without its early-outs (`s_cbranch_execz` to a block that only ends the
program) and then without the skips the code after them cannot see
(`ir::without_early_outs`, `ir::without_skips`, from VIctory-7900XTX;
`VICTORY_WAVE_FORMS=0` turns this off). The lanes outside EXEC walk the body
masked, as the console's wave does when no lane is left, and each form is
lowered by the same rules (`lockstep::fits`, `Local`), so only exact forms
are taken. The fork's further forms (wave-wide branches, local waves of
other programs) are approximate and are not taken. An early-out on a loop
stays (`VICTORY_LOOP_EARLY_OUTS=0` leaves it out as the fork did): it may be
the loop's only way out, and lanes walking on with EXEC clear would go round
until the loop limit stopped them a million turns later.

CS_ShadowUnifiedClearRaster and CS_SortRayTiles stay refused by these forms,
measured on the transformed programs (paired lanes, below, take them): the
four console waves of a ShadowUnifiedClearRaster group branch apart for
real (each wave asks whether any of its lanes passed,
`s_bcnt1` of VCC, before a whole-wave minimum and maximum, and tests VCC made
of EXEC and a uniform value), and SortRayTiles loops while any lane of the
wave has work left (`s_andn2_b64` of EXEC into an exit mask, `s_cbranch_scc1`)
between LDS atomics and barriers. The fork's 'without skips' output of the
first puts group barriers where its waves have branched apart. An exact form
needs the two halves of one console wave to meet without the rest of the
group, or a rule for loops whose finished waves may go round again with no
lane left.

### Lanes of 2-D groups on NVIDIA

Measured on an RTX 2070 SUPER (Turing, driver 621.40, `lanes.rs` in the
lab): a group up to 16 threads wide fills each 32-lane wave with a run of
32 flat ids from a multiple of 32 (lanes in quad order, which the
relabelling does not mind), with texture reads, LDS, stores or quad
operations alike. A group wider than 16 with more than one row does not:
a wave holds 16 x 2 blocks (32x32: threads 0-15 and 32-47). For those
shapes each wave takes the next console lanes from a group-shared counter
(`Lower::waves_scatter`), as victory-mix does for all groups.

### The console flow on every host

The per-lane form is wrong for a program that reads or writes a named
lane (`v_readlane`, `v_writelane`) where its lanes branch differently
(#225). Those instructions ignore EXEC on the console and a wave runs a loop
while any lane does. Per lane, a lane that left a loop or skipped a branch
is not there to be read or written, and never sees scalars set inside.
Measured before the change: a loop lane t runs t times, reading lane L in
each iteration, had 12 of 32 lanes wrong (wave32, RTX 5070 and Radeon) and
24 of 64 (wave64, Radeon); a `v_writelane` to lane 40 inside a branch only
lanes below 20 take never wrote lane 40.

Such compute programs now ask for the console flow on every host (the
second case of `CONSOLE_FLOW`): a program with `v_readlane` or
`v_writelane`, at least one branch that is not uniform, no `ds_(b)permute`
and no whole-wave sections. On a host that runs the console's wave size,
one host wave is the console wave, so ballots are the wave's own and nothing
is exchanged; any group size and LDS are fine. If the console flow cannot
lower the program, it keeps the per-lane form (`Parts::per_lane_only`), so
nothing that translated before is refused.

## Paired lanes

The last form (`emit::paired`, `lower::paired`), tried only for a console
wave64 compute program on a host without 64-lane waves that every form
above refuses; `VICTORY_WAVE_PAIRS=0` leaves those refused. Of the title's
captured compute programs these are four: CS_ShadowUnifiedClearRaster
(0c028454b58543f0), CS_DefragHeap_ComputeRelocations (02384a60fda6f2d0),
CS_SortRayTiles (a1bf269d5b19f025) and CS_AssembleComputeList
(b0d5ffa0801b7450). Their console waves branch apart (per-wave "any"
tests, loops while a lane of the wave has work, lane 63 read in loops of
different lengths), which keeps them out of the lockstep form, and they
share LDS and read lanes and masks whole, which keeps them out of the
others.

Each console wave runs as one host wave of 32 lanes: host lane n runs
console lanes n ("lower") and n + 32 ("upper"), and the group has 32
threads a console wave. The program is lowered from `structure_console` as
the console runs it: every lane through every block, the branches taken
wave-wide, EXEC guarding each vector instruction, and here each LDS read
too (a lane outside EXEC reads no LDS at an address it never set). What a
lane runs is lowered twice, the lower lane first, each time with that
lane's state swapped in (`paired::LaneState`: its values, mask bits and
their joins, its console thread), which keeps the console's order: every
lane of an instruction before any lane of the next. Scalar code and scalar
loads run once, as the console's wave runs them, and give both lanes their
results. What reads the wave reads both lanes of the host wave's threads,
with nothing exchanged:

| On the console | In paired lanes |
| --- | --- |
| a mask read whole (`s_bcnt1`, `s_ff1`, mask tests, branches on EXEC, on VCC and on a mask operation's SCC) | the lower lanes' host ballot, and the upper lanes' |
| `v_readlane_b32`, `v_readfirstlane_b32` (the first lane from EXEC's two dwords, lane 0 if none) | host lane `n & 31` of the lower or the upper value |
| `v_mbcnt`, a lane's own bit of a mask held as a number | the lane's console lane |
| a whole-wave reduction | the host wave's reduction of the lower lanes and of the upper ones, put together |
| a whole-wave prefix sum | the host wave's prefix of the lower lanes, and of the upper ones after the lower lanes' total |

So the waves of a group may branch apart: each host wave goes where some
lane of its console wave goes, and the program's own barriers are met as
the console's waves meet them. The form adds one barrier, on entry, where
each host wave takes the next console wave from an LDS count, whatever
threads the host puts in a wave; 2-D and 3-D console thread ids come back
from the console thread's flat id.

The lanes of one console wave share LDS without barriers on the console;
here they are the two lanes of a host wave's threads. A group-shared memory
fence comes before each LDS access that follows another unless both read
(a group barrier where the group is one console wave). shortcut: D3D12
orders no group-shared accesses of two threads without a group barrier;
the host is taken to run a wave's threads in step where they all go the
same way, as for the console's 32-lane programs that share LDS within a
wave. shortcut: a host wave stopped by the loop limit leaves the program
while the group's other waves may still meet at a barrier, which NVIDIA
lets them pass (it counts the threads that have not ended); D3D12 does not
promise it.

Version 0 refuses: groups that do not fill their console waves, memory
reached by address, private memory, and outside whole-wave sections DPP,
`v_permlane*`, `v_writelane`, `ds_swizzle`, `ds_(b)permute` and samples
with derivatives from quads (each would read within its lane's half, left
for a later version with tests of its own); and float sums over the wave,
which the console adds in its own order. A refused program keeps the
reason the other forms gave.

`emit::paired_tests` runs, on the RTX 2070 SUPER, hand-assembled programs
in the patterns of the four, each against a CPU model of the console's
waves: two waves walking rounds of their own while lane 63 hands an LDS
value to the next round without a barrier (5 and 9 rounds, and 1,000 and
1,001), per-wave any tests around a whole-wave minimum and maximum with
the first lane's LDS slot and a combine after the barrier, a counting sort
with loops while lanes have records, prefix sums and lane 63's wave
totals, and per-wave "any and any" counts over two rounds appended to a
list; and the console flow's one-wave cases in groups of one, two and four
console waves; and first lanes of the upper half. A broken upper ballot,
upper lane read or prefix total fails them. On the captured containers (32
lanes, exact views) no translation changed and 13 programs now translate:
the four, and nine of the same kind never bound in the logged sessions (a
second CS_DefragHeap_ComputeRelocations, CS_ShadowUnifiedClearRT, four
CS_Compact*Classification, two CS_BuildProjDecalTrail* and
CS_ScreenProbes_TraceRays_Fallback); all valid; the used compute set makes
672 pipelines with none failing, and the nine make theirs (the largest, 320
KB of DXIL, in 3.5 s).

| Program | Console VGPRs | DXIL (bytes) | Pipeline on the RTX 2070 SUPER |
| --- | --- | --- | --- |
| CS_DefragHeap_ComputeRelocations | 26 | 25,580 | 158 ms |
| CS_ShadowUnifiedClearRaster | 20 | 26,836 | 114 ms |
| CS_SortRayTiles | 20 | 9,820 | 95 ms |
| CS_AssembleComputeList | 9 | 7,684 | 30 ms |

A thread holds two lanes' VGPRs (about 52, 40, 40 and 18 values, besides
the scalars every thread keeps), against 64 registers a thread for
Turing's full occupancy (an estimate: D3D12 does not report the host's
registers); the pipeline times are first compiles, with no driver cache.

From fxceless/victory-fork#854 (PiloteCode, "Paired Wave64 compute
lowering on 32-lane GPUs"; victory-3060TI 26600c6 and p1nkman-VI,
GPL-3.0-or-later): the twin lane states swapped per vector instruction,
the paired lane reads and wave sections. Vyce's console flow already takes
every branch wave-wide, which the fork's `structure_wide` does there.

## Testing

The differential tests need the console SDK's compiler, which we do not
assume. These tests are hand-assembled GFX10 programs instead (as in
`emit::tests`), run twice:

- in the native form on an adapter with 64-lane waves (AMD RDNA, such as
  the integrated GPU of Ryzen 7000 desktop CPUs), as the reference;
- in the split form on a 32-lane adapter (NVIDIA),

then compared buffer for buffer. `emit::host::Host::on(name)` and
`VICTORY_ADAPTER=<part of the adapter's name>` pick the adapter;
`emit::tests::each_adapter_gets_what_it_runs` checks every adapter in the
machine (on a Ryzen 7800X3D with an RTX 5070: the Radeon reports 32-64 and
takes `WaveSize(64)`, the NVIDIA reports 32-32).

## Steps

1. ~~Translate for the host's wave range; cache by it; `--host-lanes`.~~
2. ~~Adapter selection for tests and tools.~~
3. Measure the title's programs by reason (`--host-lanes 32-32`), to
   order steps 4 to 6.
4. ~~In-half programs: console-lane programs whose cross-lane operations
   all stay inside a half, with no mask read whole.~~ No new lane assignment
   was needed (see above). A later widening is LDS in multi-wave groups,
   whose waves already meet at barriers.
5. ~~The console flow and the one-wave exchange (ballots, readlane,
   readfirstlane, writelane, mbcnt, divergent branches and loops).~~
6. ~~Multi-wave groups: in step where their waves branch alike; local
   waves where lanes only meet within 32; paired lanes where their waves
   branch apart (CS_DefragHeap_ComputeRelocations,
   CS_ShadowUnifiedClearRaster, CS_SortRayTiles, CS_AssembleComputeList).~~

## Credit

The approach of pairing two 32-lane waves through groupshared memory, and
the finding that NVIDIA forms waves from non-consecutive thread ids in some
3-D groups, come from XCL's design notes (`docs/wave64-on-wave32.md` in
github.com/ivall/xcl and its fork Bathyal). XCL is GPL-2.0-only. Nothing
here is copied from its code.

Paired lanes are ported from fxceless/victory-fork#854 by PiloteCode
(victory-3060TI 26600c6, p1nkman-VI), GPL-3.0-or-later.
