//! Console wave64 compute programs whose thread groups hold several waves, on
//! a host of 32-lane waves: all the group's host waves run the console flow in
//! step, so that the two halves of each console wave meet at group barriers
//! (`lower::CONSOLE_FLOW`, which alone is for groups of one wave).
//!
//! That holds where every wave of the group takes every branch the same way,
//! or takes it only to skip code its EXEC has no lane for: branches on EXEC
//! (a wave with no lane there runs the code with every vector instruction
//! guarded off), and on scalar tests of values that are the same in the whole
//! group (constants, root data, the group's number, and what is worked out or
//! loaded from them).

use std::collections::{HashMap, HashSet};

use crate::gfx10::Fields;
use crate::io::{Input, Io};
use crate::ir::{EXEC, Exit, Program, Reg, VCC, Value};

/// Whether the waves of a group may run `p` in step.
pub(crate) fn fits(p: &Program, io: &Io) -> bool {
    let differs = differing(p, io);
    p.blocks.iter().all(|block| {
        let Exit::Branch { .. } = block.exit else {
            return !matches!(block.exit, Exit::Indirect);
        };
        let Some(inst) = block.insts.last() else { return false };
        match inst.name() {
            "s_cbranch_execz" | "s_cbranch_execnz" => true,
            "s_cbranch_scc0" | "s_cbranch_scc1" => {
                inst.read(Reg::Scc).is_some_and(|v| !differs.contains(&v))
            }
            _ => false,
        }
    })
}

/// The scalar values that may differ between the waves of a group.
fn differing(p: &Program, io: &Io) -> HashSet<Value> {
    let inputs: HashMap<Value, Input> = io.inputs.iter().copied().collect();
    let mut differs: HashSet<Value> = p
        .inputs
        .iter()
        .filter(|(reg, v)| match *reg {
            Reg::S(c) if matches!(c, EXEC | VCC) || c == EXEC + 1 || c == VCC + 1 => true,
            Reg::S(_) | Reg::Scc => matches!(inputs.get(v), Some(Input::GroupInfo)),
            _ => true,
        })
        .map(|&(_, v)| v)
        .collect();
    loop {
        let before = differs.len();
        for block in &p.blocks {
            for phi in &block.phis {
                if phi.args.iter().any(|a| differs.contains(a)) {
                    differs.insert(phi.value);
                }
            }
            for inst in &block.insts {
                let scalar = matches!(
                    inst.fields,
                    Fields::Sop { .. } | Fields::Sopp { .. } | Fields::Smem { .. }
                );
                // A lane's work (compares, lane reads), or what reads such.
                let from = |&(r, v): &(Reg, Value)| {
                    matches!(r, Reg::V(_)) || differs.contains(&v)
                };
                if !scalar || inst.reads.iter().any(from) {
                    differs.extend(inst.writes.iter().map(|w| w.1));
                }
            }
        }
        if differs.len() == before {
            return differs;
        }
    }
}
