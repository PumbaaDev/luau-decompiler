use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use crate::parser::opcodes::LuauOpcode;
use crate::parser::types::*;

/// A basic block in the control flow graph
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub id: usize,
    /// Instruction range [start, end) in the proto's code array
    pub start: usize,
    pub end: usize,
    pub successors: Vec<usize>,
    pub predecessors: Vec<usize>,
}

/// Complete control flow graph for a function
#[derive(Debug)]
pub struct ControlFlowGraph {
    pub blocks: BTreeMap<usize, BasicBlock>,
    pub entry: usize,
    pub pc_to_block: HashMap<usize, usize>,
}

impl ControlFlowGraph {
    /// Build a CFG from a proto's instruction stream
    pub fn build(proto: &Proto) -> Self {
        let code = &proto.code;
        if code.is_empty() {
            return Self {
                blocks: BTreeMap::new(),
                entry: 0,
                pc_to_block: HashMap::new(),
            };
        }

        // Step 1: Find block leaders
        let mut leaders = BTreeSet::new();
        leaders.insert(0);

        let mut pc = 0usize;
        while pc < code.len() {
            let insn = code[pc];
            let op = LuauOpcode::from_u8(insn_op(insn));
            let d = insn_d(insn);
            let e = insn_e(insn);
            let next_pc = if op.has_aux() { pc + 2 } else { pc + 1 };

            match op {
                LuauOpcode::Jump | LuauOpcode::JumpBack => {
                    let target = (pc as i32 + d as i32 + 1) as usize;
                    if target < code.len() {
                        leaders.insert(target);
                    }
                    if next_pc < code.len() {
                        leaders.insert(next_pc);
                    }
                }
                LuauOpcode::JumpX => {
                    let target = (pc as i32 + e + 1) as usize;
                    if target < code.len() {
                        leaders.insert(target);
                    }
                    if next_pc < code.len() {
                        leaders.insert(next_pc);
                    }
                }
                LuauOpcode::JumpIf | LuauOpcode::JumpIfNot
                | LuauOpcode::JumpIfEq | LuauOpcode::JumpIfLE | LuauOpcode::JumpIfLT
                | LuauOpcode::JumpIfNotEq | LuauOpcode::JumpIfNotLE | LuauOpcode::JumpIfNotLT
                | LuauOpcode::JumpXEqKNil | LuauOpcode::JumpXEqKB
                | LuauOpcode::JumpXEqKN | LuauOpcode::JumpXEqKS
                | LuauOpcode::ForNLoop | LuauOpcode::ForGLoop
                | LuauOpcode::Deprecated61 => {
                    let target = (pc as i32 + d as i32 + 1) as usize;
                    if target < code.len() {
                        leaders.insert(target);
                    }
                    if next_pc < code.len() {
                        leaders.insert(next_pc);
                    }
                }
                // For-prep opcodes must start their own block so the structuring
                // pass can identify them via try_match_for_loop (which checks the
                // first instruction of each block).
                LuauOpcode::ForNPrep | LuauOpcode::ForGPrep
                | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext => {
                    leaders.insert(pc); // prep itself is a leader
                    let target = (pc as i32 + d as i32 + 1) as usize;
                    if target < code.len() {
                        leaders.insert(target);
                    }
                    if next_pc < code.len() {
                        leaders.insert(next_pc);
                    }
                }
                LuauOpcode::Return => {
                    if next_pc < code.len() {
                        leaders.insert(next_pc);
                    }
                }
                _ => {}
            }
            pc = next_pc;
        }

        // Step 2: Create blocks
        let leader_vec: Vec<usize> = leaders.iter().copied().collect();
        let mut blocks = BTreeMap::new();
        let mut pc_to_block = HashMap::new();

        for (i, &start) in leader_vec.iter().enumerate() {
            let end = if i + 1 < leader_vec.len() {
                leader_vec[i + 1]
            } else {
                code.len()
            };
            let block = BasicBlock {
                id: start,
                start,
                end,
                successors: Vec::new(),
                predecessors: Vec::new(),
            };
            for p in start..end {
                pc_to_block.insert(p, start);
            }
            blocks.insert(start, block);
        }

        // Step 3: Connect edges
        for &start in &leader_vec {
            // Bounds-checked lookup — `start` is always in `blocks` for
            // well-formed input (leader_vec seeds blocks), but prefer
            // graceful skip on malformed state.
            let block_end = match blocks.get(&start) {
                Some(b) => b.end,
                None => continue,
            };
            if block_end == 0 {
                continue;
            }

            // Find last real instruction (skip back past AUX words)
            // If instruction at (last_pc - 1) has_aux(), then code[last_pc] is its AUX word,
            // so the last real instruction is at (last_pc - 1)
            let mut last_pc = block_end - 1;
            if last_pc > start {
                // Check if the previous instruction has an AUX word, making current position an AUX
                let check_pc = last_pc - 1;
                if let Some(&check_insn) = code.get(check_pc) {
                    let check_op = LuauOpcode::from_u8(insn_op(check_insn));
                    if check_op.has_aux() {
                        last_pc = check_pc;
                    }
                }
            }

            // Bounds-checked — malformed `block_end` could exceed code length.
            let insn = match code.get(last_pc) {
                Some(&i) => i,
                None => continue,
            };
            let op = LuauOpcode::from_u8(insn_op(insn));
            let d = insn_d(insn);
            let e = insn_e(insn);

            let mut successors = Vec::new();
            match op {
                LuauOpcode::Jump | LuauOpcode::JumpBack => {
                    let target = (last_pc as i32 + d as i32 + 1) as usize;
                    successors.push(target);
                }
                LuauOpcode::JumpX => {
                    let target = (last_pc as i32 + e + 1) as usize;
                    successors.push(target);
                }
                LuauOpcode::JumpIf | LuauOpcode::JumpIfNot
                | LuauOpcode::JumpIfEq | LuauOpcode::JumpIfLE | LuauOpcode::JumpIfLT
                | LuauOpcode::JumpIfNotEq | LuauOpcode::JumpIfNotLE | LuauOpcode::JumpIfNotLT
                | LuauOpcode::JumpXEqKNil | LuauOpcode::JumpXEqKB
                | LuauOpcode::JumpXEqKN | LuauOpcode::JumpXEqKS => {
                    if block_end < code.len() {
                        successors.push(block_end); // fallthrough
                    }
                    let target = (last_pc as i32 + d as i32 + 1) as usize;
                    successors.push(target); // branch
                }
                LuauOpcode::ForNPrep | LuauOpcode::ForGPrep
                | LuauOpcode::ForGPrepINext | LuauOpcode::ForGPrepNext => {
                    if block_end < code.len() {
                        successors.push(block_end); // into loop
                    }
                    let target = (last_pc as i32 + d as i32 + 1) as usize;
                    successors.push(target); // skip loop
                }
                LuauOpcode::ForNLoop | LuauOpcode::ForGLoop | LuauOpcode::Deprecated61 => {
                    let target = (last_pc as i32 + d as i32 + 1) as usize;
                    successors.push(target); // loop back
                    if block_end < code.len() {
                        successors.push(block_end); // exit
                    }
                }
                LuauOpcode::Return => {} // no successors
                _ => {
                    if block_end < code.len() {
                        successors.push(block_end);
                    }
                }
            }

            let valid: Vec<usize> = successors.into_iter().filter(|s| blocks.contains_key(s)).collect();
            // Bounds-checked: `start` comes from `leader_vec` which seeds
            // `blocks`; a missing entry here indicates malformed CFG state
            // (e.g. a block was removed mid-build). Skip rather than panic.
            if let Some(b) = blocks.get_mut(&start) {
                b.successors = valid.clone();
            } else {
                continue;
            }
            for &succ in &valid {
                // `valid` was filtered via `blocks.contains_key`, so a missing
                // entry would only happen under concurrent mutation, which
                // cannot occur (this is a single-threaded builder). Still,
                // prefer a graceful fallback over `.unwrap()` for robustness.
                if let Some(b) = blocks.get_mut(&succ) {
                    b.predecessors.push(start);
                }
            }
        }

        Self {
            blocks,
            entry: 0,
            pc_to_block,
        }
    }

    pub fn compute_dominators(&self) -> HashMap<usize, usize> {
        let mut idom: HashMap<usize, usize> = HashMap::new();
        idom.insert(self.entry, self.entry);
        let rpo = self.reverse_postorder();
        let rpo_index: HashMap<usize, usize> = rpo.iter().enumerate().map(|(i, &id)| (id, i)).collect();

        let mut changed = true;
        while changed {
            changed = false;
            for &b in &rpo {
                if b == self.entry { continue; }
                let preds = &self.blocks[&b].predecessors;
                let mut new_idom: Option<usize> = None;
                for &p in preds {
                    if !idom.contains_key(&p) { continue; }
                    new_idom = Some(match new_idom {
                        None => p,
                        Some(cur) => self.intersect(&idom, &rpo_index, cur, p),
                    });
                }
                if let Some(new) = new_idom {
                    if idom.get(&b) != Some(&new) {
                        idom.insert(b, new);
                        changed = true;
                    }
                }
            }
        }
        idom
    }

    fn intersect(&self, idom: &HashMap<usize, usize>, rpo_index: &HashMap<usize, usize>, mut a: usize, mut b: usize) -> usize {
        while a != b {
            while rpo_index.get(&a).copied().unwrap_or(usize::MAX) > rpo_index.get(&b).copied().unwrap_or(usize::MAX) {
                a = *idom.get(&a).unwrap_or(&a);
            }
            while rpo_index.get(&b).copied().unwrap_or(usize::MAX) > rpo_index.get(&a).copied().unwrap_or(usize::MAX) {
                b = *idom.get(&b).unwrap_or(&b);
            }
        }
        a
    }

    pub fn reverse_postorder(&self) -> Vec<usize> {
        let mut visited = BTreeSet::new();
        let mut order = Vec::new();
        self.rpo_visit(self.entry, &mut visited, &mut order);
        order.reverse();
        order
    }

    fn rpo_visit(&self, node: usize, visited: &mut BTreeSet<usize>, order: &mut Vec<usize>) {
        if !visited.insert(node) { return; }
        if let Some(block) = self.blocks.get(&node) {
            for &succ in &block.successors {
                self.rpo_visit(succ, visited, order);
            }
        }
        order.push(node);
    }

    pub fn find_loops(&self) -> Vec<NaturalLoop> {
        let idom = self.compute_dominators();
        let mut loops = Vec::new();
        for (&block_id, block) in &self.blocks {
            for &succ in &block.successors {
                if self.dominates(&idom, succ, block_id) {
                    let body = self.collect_loop_body(succ, block_id);
                    loops.push(NaturalLoop { header: succ, back_edge_source: block_id, body });
                }
            }
        }
        loops
    }

    fn dominates(&self, idom: &HashMap<usize, usize>, a: usize, mut b: usize) -> bool {
        loop {
            if b == a { return true; }
            match idom.get(&b) {
                Some(&dom) if dom != b => b = dom,
                _ => return false,
            }
        }
    }

    fn collect_loop_body(&self, header: usize, back_edge_src: usize) -> BTreeSet<usize> {
        let mut body = BTreeSet::new();
        body.insert(header);
        if header == back_edge_src { return body; }
        body.insert(back_edge_src);
        let mut worklist = VecDeque::new();
        worklist.push_back(back_edge_src);
        while let Some(node) = worklist.pop_front() {
            if let Some(block) = self.blocks.get(&node) {
                for &pred in &block.predecessors {
                    if body.insert(pred) {
                        worklist.push_back(pred);
                    }
                }
            }
        }
        body
    }

    /// Find the immediate post-dominator / merge point for a conditional branch.
    /// This is the first block reachable from BOTH successors.
    pub fn find_merge_point(&self, true_target: usize, false_target: usize) -> Option<usize> {
        // BFS from both targets, find first overlap
        let mut visited_true = BTreeSet::new();
        let mut visited_false = BTreeSet::new();
        let mut queue_true = VecDeque::new();
        let mut queue_false = VecDeque::new();

        visited_true.insert(true_target);
        visited_false.insert(false_target);
        queue_true.push_back(true_target);
        queue_false.push_back(false_target);

        // Check if one is directly the other
        if true_target == false_target {
            return Some(true_target);
        }

        // Alternating BFS
        for _ in 0..self.blocks.len() * 2 {
            if let Some(node) = queue_true.pop_front() {
                if visited_false.contains(&node) {
                    return Some(node);
                }
                if let Some(block) = self.blocks.get(&node) {
                    for &succ in &block.successors {
                        if visited_true.insert(succ) {
                            queue_true.push_back(succ);
                        }
                    }
                }
            }
            if let Some(node) = queue_false.pop_front() {
                if visited_true.contains(&node) {
                    return Some(node);
                }
                if let Some(block) = self.blocks.get(&node) {
                    for &succ in &block.successors {
                        if visited_false.insert(succ) {
                            queue_false.push_back(succ);
                        }
                    }
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
pub struct NaturalLoop {
    pub header: usize,
    pub back_edge_source: usize,
    pub body: BTreeSet<usize>,
}
