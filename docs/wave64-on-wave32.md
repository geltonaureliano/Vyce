# Console wave64 compute on 32-lane hosts

Status: steps 1, 2, 4 and 5 are in. On NVIDIA, wave64 programs whose lanes
read only their own half run as two 32-lane waves. Groups of one console
wave also run in the console flow: masks read whole, lanes of the other
half, divergent branches, loops and single-wave LDS. Still open: groups of
several console waves, whole-wave sections and `ds_permute_b32` scatter.

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

### Forms without early-outs or skips

A wave64 compute program that the 32-lane forms refuse is tried again
without its early-outs (`s_cbranch_execz` to a block that only ends the
program) and then without the skips the code after them cannot see
(`ir::without_early_outs`, `ir::without_skips`, from VIctory-7900XTX;
`VICTORY_WAVE_FORMS=0` turns this off). The lanes outside EXEC walk the body
masked, as the console's wave does when no lane is left, and each form is
lowered by the same rules (`lockstep::fits`, `Local`), so only exact forms
are taken. The fork's further forms (wave-wide branches, local waves of
other programs) are approximate and are not taken.

CS_ShadowUnifiedClearRaster and CS_SortRayTiles stay refused, measured on
the transformed programs: the four console waves of a ShadowUnifiedClearRaster
group branch apart for real (each wave asks whether any of its lanes passed,
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
   waves where lanes only meet within 32.~~ Left: CS_DefragHeap,
   CS_ShadowUnifiedClearRaster (branches on lanes that differ per wave) and
   CS_SortRayTiles (see "Forms without early-outs or skips").

## Credit

The approach of pairing two 32-lane waves through groupshared memory, and
the finding that NVIDIA forms waves from non-consecutive thread ids in some
3-D groups, come from XCL's design notes (`docs/wave64-on-wave32.md` in
github.com/ivall/xcl and its fork Bathyal). XCL is GPL-2.0-only. Nothing
here is copied from its code.
