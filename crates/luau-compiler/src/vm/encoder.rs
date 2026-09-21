//! Encode an [`crate::ir::Program`] into a [`Module`] of stack-VM bytecode.
//!
//! Name resolution, scope tracking, and jump backpatching live here. Each
//! IR proto becomes one [`EncodedProto`] with a flat instruction stream;
//! nested protos are registered first so a parent's `Closure` op can
//! reference them by index.

use std::collections::HashMap;

use crate::ir::{BinOp, Expr, LValue, Program, Stmt, TableField, UnOp};

use super::opcodes::{BinSubOp, Op, UnSubOp};
use super::{emit_instr, patch_a, Const, ConstKey, EncodedProto, Module, INSTR_WIDTH};

pub fn encode(program: &Program) -> Result<Module, String> {
    use super::StringState;
    let mut enc = Encoder::default();
    enc.module.protos = vec![EncodedProto::default(); program.protos.len()];
    enc.encode_proto(program, 0)?;
    let n_protos = enc.module.protos.len();
    let n_const = enc.module.constants.len();
    enc.module.code_states = vec![StringState::Plain; n_protos];
    enc.module.const_states = vec![StringState::Plain; n_const];
    Ok(enc.module)
}

#[derive(Default)]
struct Encoder {
    module: Module,
    const_pool: HashMap<ConstKey, u16>,
    /// Stack of in-progress proto frames, outermost first. The current proto
    /// being encoded is always at the top.
    frames: Vec<Frame>,
}

struct Frame {
    /// Index of the proto this frame is encoding into. Currently consumed
    /// only by debug logging — the encoder uses `frames.last()` to access
    /// the current frame's data — but Phase 5+ passes that walk the frame
    /// stack from the outside need it.
    #[allow(dead_code)]
    proto_idx: usize,
    code: Vec<u8>,
    scopes: Vec<Scope>,
    upvalues: Vec<UpvalSpec>,
    /// Next local slot to allocate within this proto.
    next_slot: u16,
    /// Max local slot used (= num_locals for the encoded proto).
    max_slot: u16,
    is_vararg: bool,
    num_params: u16,
    /// Per-loop list of code offsets where `Jump` instructions need their A
    /// operand backpatched to the loop's exit point. Pushed on loop entry,
    /// popped on loop exit.
    break_patches: Vec<Vec<usize>>,
    /// Same shape for `continue` jumps — backpatched to loop top.
    continue_patches: Vec<Vec<usize>>,
}

#[derive(Default)]
struct Scope {
    locals: Vec<LocalBinding>,
    /// `next_slot` value at scope entry — used to roll back local allocation
    /// when the scope ends.
    base_slot: u16,
}

struct LocalBinding {
    name: String,
    slot: u16,
}

struct UpvalSpec {
    /// 0 = the parent's local slot at `index`. 1 = the parent's upvalue at `index`.
    kind: u8,
    index: u16,
    name: String,
}

impl Encoder {
    fn encode_proto(&mut self, program: &Program, idx: usize) -> Result<(), String> {
        let ir = &program.protos[idx];
        let frame = Frame {
            proto_idx: idx,
            code: Vec::new(),
            scopes: vec![Scope::default()],
            upvalues: Vec::new(),
            next_slot: 0,
            max_slot: 0,
            is_vararg: ir.is_vararg,
            num_params: ir.num_params(),
            break_patches: Vec::new(),
            continue_patches: Vec::new(),
        };
        self.frames.push(frame);

        // Reserve local slots 0..num_params for parameters. The runtime binds
        // args[i] -> locals[i] at call time, so no init instructions emit here.
        for name in &ir.param_names {
            self.alloc_local(name.clone());
        }

        self.encode_block(program, &ir.body)?;

        // Implicit final return so the dispatcher always halts cleanly.
        emit_instr(self.code_mut(), Op::Return, 0, 0);

        let frame = self.frames.pop().expect("frame stack underflow");
        let encoded = EncodedProto {
            num_params: frame.num_params,
            num_locals: frame.max_slot,
            num_upvalues: frame.upvalues.len() as u16,
            is_vararg: frame.is_vararg,
            code: frame.code,
            upvalue_specs: frame
                .upvalues
                .iter()
                .map(|u| (u.kind, u.index))
                .collect(),
            operand_key: 0,
        };
        self.module.protos[idx] = encoded;
        Ok(())
    }

    fn encode_block(&mut self, program: &Program, stmts: &[Stmt]) -> Result<(), String> {
        self.push_scope();
        for stmt in stmts {
            self.encode_stmt(program, stmt)?;
        }
        self.pop_scope();
        Ok(())
    }

    fn encode_stmt(&mut self, program: &Program, stmt: &Stmt) -> Result<(), String> {
        match stmt {
            Stmt::Local { names, values } => self.encode_local(program, names, values),
            Stmt::Assign { targets, values } => self.encode_assign(program, targets, values),
            Stmt::ExprStmt(e) => {
                self.encode_expr(program, e)?;
                // Drop the single value the expression pushed.
                emit_instr(self.code_mut(), Op::Pop, 1, 0);
                Ok(())
            }
            Stmt::If { branches, else_body } => self.encode_if(program, branches, else_body),
            Stmt::While { cond, body } => self.encode_while(program, cond, body),
            Stmt::Repeat { body, cond } => self.encode_repeat(program, body, cond),
            Stmt::NumericFor {
                var,
                start,
                stop,
                step,
                body,
            } => self.encode_numeric_for(program, var, start, stop, step.as_ref(), body),
            Stmt::GenericFor { .. } => {
                Err("generic for not yet supported (Phase 2)".into())
            }
            Stmt::Return(exprs) => {
                let n = exprs.len();
                for e in exprs {
                    self.encode_expr(program, e)?;
                }
                emit_instr(self.code_mut(), Op::Return, n as i16, 0);
                Ok(())
            }
            Stmt::Break => {
                let pos = self.code_len();
                emit_instr(self.code_mut(), Op::Jump, 0, 0);
                let frame = self.frame_mut();
                let top = frame
                    .break_patches
                    .last_mut()
                    .ok_or_else(|| "break outside loop".to_string())?;
                top.push(pos);
                Ok(())
            }
            Stmt::Continue => {
                let pos = self.code_len();
                emit_instr(self.code_mut(), Op::Jump, 0, 0);
                let frame = self.frame_mut();
                let top = frame
                    .continue_patches
                    .last_mut()
                    .ok_or_else(|| "continue outside loop".to_string())?;
                top.push(pos);
                Ok(())
            }
            Stmt::Do(body) => self.encode_block(program, body),
            Stmt::LocalFunction { name, proto_idx } => {
                // Pre-allocate the local slot so the function body can resolve
                // self-references as an upvalue (Lua semantics for
                // `local function`).
                let slot = self.alloc_local(name.clone());
                if self.module.protos[*proto_idx].code.is_empty() {
                    self.encode_proto(program, *proto_idx)?;
                }
                let upvals = self.module.protos[*proto_idx].upvalue_specs.clone();
                emit_instr(
                    self.code_mut(),
                    Op::Closure,
                    *proto_idx as i16,
                    upvals.len() as i16,
                );
                for (kind, index) in upvals {
                    emit_instr(self.code_mut(), Op::ClosureUpval, kind as i16, index as i16);
                }
                emit_instr(self.code_mut(), Op::StoreLocal, slot as i16, 0);
                Ok(())
            }
        }
    }

    fn encode_local(
        &mut self,
        program: &Program,
        names: &[String],
        values: &[Expr],
    ) -> Result<(), String> {
        // Evaluate values left-to-right.
        for v in values {
            self.encode_expr(program, v)?;
        }
        // Pad with nils if there are fewer values than names.
        if values.len() < names.len() {
            for _ in 0..(names.len() - values.len()) {
                emit_instr(self.code_mut(), Op::PushNil, 0, 0);
            }
        } else if values.len() > names.len() {
            let extra = values.len() - names.len();
            emit_instr(self.code_mut(), Op::Pop, extra as i16, 0);
        }
        // Allocate slots and store right-to-left (top of stack = last name).
        let mut slots = Vec::with_capacity(names.len());
        for name in names {
            let slot = self.alloc_local(name.clone());
            slots.push(slot);
        }
        for slot in slots.iter().rev() {
            emit_instr(self.code_mut(), Op::StoreLocal, *slot as i16, 0);
        }
        Ok(())
    }

    fn encode_assign(
        &mut self,
        program: &Program,
        targets: &[LValue],
        values: &[Expr],
    ) -> Result<(), String> {
        // Evaluate all RHS first so swaps like `a, b = b, a` are correct.
        for v in values {
            self.encode_expr(program, v)?;
        }
        if values.len() < targets.len() {
            for _ in 0..(targets.len() - values.len()) {
                emit_instr(self.code_mut(), Op::PushNil, 0, 0);
            }
        } else if values.len() > targets.len() {
            let extra = values.len() - targets.len();
            emit_instr(self.code_mut(), Op::Pop, extra as i16, 0);
        }
        // Now stack has N values for N targets, top = last target.
        // For each target right-to-left: pop value (already on top) and store.
        // For Field/Index targets the obj/key need to be evaluated AFTER the
        // RHS values are computed, so we stash the value into a temp local,
        // evaluate the receiver, then re-load.
        // Simple/correct approach: snapshot all values into hidden locals,
        // then assign one at a time.
        let mut temp_slots = Vec::with_capacity(targets.len());
        for _ in 0..targets.len() {
            let slot = self.alloc_anon_local();
            temp_slots.push(slot);
        }
        for slot in temp_slots.iter().rev() {
            emit_instr(self.code_mut(), Op::StoreLocal, *slot as i16, 0);
        }
        for (slot, target) in temp_slots.iter().zip(targets.iter()) {
            self.encode_lvalue_store(program, target, *slot)?;
        }
        // Anonymous locals stay allocated until scope ends; that's fine.
        Ok(())
    }

    fn encode_lvalue_store(
        &mut self,
        program: &Program,
        lv: &LValue,
        value_slot: u16,
    ) -> Result<(), String> {
        match lv {
            LValue::Local(name) => {
                if let Some(slot) = self.resolve_local(name) {
                    emit_instr(self.code_mut(), Op::LoadLocal, value_slot as i16, 0);
                    emit_instr(self.code_mut(), Op::StoreLocal, slot as i16, 0);
                } else if let Some(uv) = self.resolve_upvalue(name)? {
                    emit_instr(self.code_mut(), Op::LoadLocal, value_slot as i16, 0);
                    emit_instr(self.code_mut(), Op::StoreUpval, uv as i16, 0);
                } else {
                    let k = self.intern_string(name);
                    emit_instr(self.code_mut(), Op::LoadLocal, value_slot as i16, 0);
                    emit_instr(self.code_mut(), Op::StoreGlobal, k as i16, 0);
                }
                Ok(())
            }
            LValue::Global(name) => {
                if let Some(slot) = self.resolve_local(name) {
                    emit_instr(self.code_mut(), Op::LoadLocal, value_slot as i16, 0);
                    emit_instr(self.code_mut(), Op::StoreLocal, slot as i16, 0);
                } else if let Some(uv) = self.resolve_upvalue(name)? {
                    emit_instr(self.code_mut(), Op::LoadLocal, value_slot as i16, 0);
                    emit_instr(self.code_mut(), Op::StoreUpval, uv as i16, 0);
                } else {
                    let k = self.intern_string(name);
                    emit_instr(self.code_mut(), Op::LoadLocal, value_slot as i16, 0);
                    emit_instr(self.code_mut(), Op::StoreGlobal, k as i16, 0);
                }
                Ok(())
            }
            LValue::Field { obj, name } => {
                self.encode_expr(program, obj)?;
                emit_instr(self.code_mut(), Op::LoadLocal, value_slot as i16, 0);
                let k = self.intern_string(name);
                emit_instr(self.code_mut(), Op::SetField, k as i16, 0);
                Ok(())
            }
            LValue::Index { obj, key } => {
                self.encode_expr(program, obj)?;
                self.encode_expr(program, key)?;
                emit_instr(self.code_mut(), Op::LoadLocal, value_slot as i16, 0);
                emit_instr(self.code_mut(), Op::SetIndex, 0, 0);
                Ok(())
            }
        }
    }

    fn encode_if(
        &mut self,
        program: &Program,
        branches: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
    ) -> Result<(), String> {
        let mut end_jumps = Vec::new();
        for (i, (cond, body)) in branches.iter().enumerate() {
            self.encode_expr(program, cond)?;
            let jif_pos = self.code_len();
            emit_instr(self.code_mut(), Op::JumpIfFalse, 0, 0);
            self.encode_block(program, body)?;
            let has_more = i + 1 < branches.len() || else_body.is_some();
            if has_more {
                let j_pos = self.code_len();
                emit_instr(self.code_mut(), Op::Jump, 0, 0);
                end_jumps.push(j_pos);
            }
            let after_body = self.code_len();
            self.patch_jump_to(jif_pos, after_body);
        }
        if let Some(else_stmts) = else_body {
            self.encode_block(program, else_stmts)?;
        }
        let after_all = self.code_len();
        for j in end_jumps {
            self.patch_jump_to(j, after_all);
        }
        Ok(())
    }

    fn encode_while(
        &mut self,
        program: &Program,
        cond: &Expr,
        body: &[Stmt],
    ) -> Result<(), String> {
        let loop_top = self.code_len();
        self.encode_expr(program, cond)?;
        let exit_jump = self.code_len();
        emit_instr(self.code_mut(), Op::JumpIfFalse, 0, 0);

        self.frame_mut().break_patches.push(Vec::new());
        self.frame_mut().continue_patches.push(Vec::new());
        self.encode_block(program, body)?;
        let continues = self.frame_mut().continue_patches.pop().unwrap();
        let breaks = self.frame_mut().break_patches.pop().unwrap();

        // Jump back to top.
        let back_pos = self.code_len();
        emit_instr(self.code_mut(), Op::Jump, 0, 0);
        self.patch_jump_to(back_pos, loop_top);

        let after_loop = self.code_len();
        self.patch_jump_to(exit_jump, after_loop);
        for c in continues {
            self.patch_jump_to(c, loop_top);
        }
        for b in breaks {
            self.patch_jump_to(b, after_loop);
        }
        Ok(())
    }

    fn encode_repeat(
        &mut self,
        program: &Program,
        body: &[Stmt],
        cond: &Expr,
    ) -> Result<(), String> {
        let loop_top = self.code_len();
        self.frame_mut().break_patches.push(Vec::new());
        self.frame_mut().continue_patches.push(Vec::new());
        self.encode_block(program, body)?;
        let continues = self.frame_mut().continue_patches.pop().unwrap();
        let breaks = self.frame_mut().break_patches.pop().unwrap();
        let cond_pc = self.code_len();
        self.encode_expr(program, cond)?;
        let back_pos = self.code_len();
        emit_instr(self.code_mut(), Op::JumpIfFalse, 0, 0);
        self.patch_jump_to(back_pos, loop_top);
        let after_loop = self.code_len();
        for c in continues {
            self.patch_jump_to(c, cond_pc);
        }
        for b in breaks {
            self.patch_jump_to(b, after_loop);
        }
        Ok(())
    }

    fn encode_numeric_for(
        &mut self,
        program: &Program,
        var: &str,
        start: &Expr,
        stop: &Expr,
        step: Option<&Expr>,
        body: &[Stmt],
    ) -> Result<(), String> {
        // Desugar to: hidden _stop, _step + visible loop var + while-style loop.
        self.push_scope();
        let stop_slot = self.alloc_anon_local();
        let step_slot = self.alloc_anon_local();
        let var_slot = self.alloc_local(var.to_string());

        // var = start
        self.encode_expr(program, start)?;
        emit_instr(self.code_mut(), Op::StoreLocal, var_slot as i16, 0);
        // _stop = stop
        self.encode_expr(program, stop)?;
        emit_instr(self.code_mut(), Op::StoreLocal, stop_slot as i16, 0);
        // _step = step or 1
        match step {
            Some(e) => self.encode_expr(program, e)?,
            None => {
                let k = self.intern_number(1.0);
                emit_instr(self.code_mut(), Op::PushConst, k as i16, 0);
            }
        }
        emit_instr(self.code_mut(), Op::StoreLocal, step_slot as i16, 0);

        let loop_top = self.code_len();
        // Condition: (step >= 0 and var <= stop) or (step < 0 and var >= stop)
        // Emit:
        //   tmp = step >= 0
        //   if not tmp: jump branch_neg
        //   tmp2 = var <= stop
        //   if not tmp2: jump exit
        //   jump body
        // branch_neg:
        //   tmp3 = var >= stop
        //   if not tmp3: jump exit
        // body:
        //   ...
        //   var = var + step
        //   jump loop_top
        // exit:
        let zero_const = self.intern_number(0.0);

        // step >= 0 ?
        emit_instr(self.code_mut(), Op::LoadLocal, step_slot as i16, 0);
        emit_instr(self.code_mut(), Op::PushConst, zero_const as i16, 0);
        emit_instr(self.code_mut(), Op::BinOp, BinSubOp::Ge as i16, 0);
        let to_neg = self.code_len();
        emit_instr(self.code_mut(), Op::JumpIfFalse, 0, 0);

        // step>=0 branch: var <= stop ?
        emit_instr(self.code_mut(), Op::LoadLocal, var_slot as i16, 0);
        emit_instr(self.code_mut(), Op::LoadLocal, stop_slot as i16, 0);
        emit_instr(self.code_mut(), Op::BinOp, BinSubOp::Le as i16, 0);
        let exit_from_pos = self.code_len();
        emit_instr(self.code_mut(), Op::JumpIfFalse, 0, 0);
        let to_body = self.code_len();
        emit_instr(self.code_mut(), Op::Jump, 0, 0);

        // negative-step branch
        let neg_branch = self.code_len();
        self.patch_jump_to(to_neg, neg_branch);
        emit_instr(self.code_mut(), Op::LoadLocal, var_slot as i16, 0);
        emit_instr(self.code_mut(), Op::LoadLocal, stop_slot as i16, 0);
        emit_instr(self.code_mut(), Op::BinOp, BinSubOp::Ge as i16, 0);
        let exit_from_neg = self.code_len();
        emit_instr(self.code_mut(), Op::JumpIfFalse, 0, 0);

        // body
        let body_pos = self.code_len();
        self.patch_jump_to(to_body, body_pos);

        self.frame_mut().break_patches.push(Vec::new());
        self.frame_mut().continue_patches.push(Vec::new());
        for s in body {
            self.encode_stmt(program, s)?;
        }
        let continues = self.frame_mut().continue_patches.pop().unwrap();
        let breaks = self.frame_mut().break_patches.pop().unwrap();

        let incr_pos = self.code_len();
        for c in continues {
            self.patch_jump_to(c, incr_pos);
        }

        // var = var + step
        emit_instr(self.code_mut(), Op::LoadLocal, var_slot as i16, 0);
        emit_instr(self.code_mut(), Op::LoadLocal, step_slot as i16, 0);
        emit_instr(self.code_mut(), Op::BinOp, BinSubOp::Add as i16, 0);
        emit_instr(self.code_mut(), Op::StoreLocal, var_slot as i16, 0);

        let back = self.code_len();
        emit_instr(self.code_mut(), Op::Jump, 0, 0);
        self.patch_jump_to(back, loop_top);

        let after = self.code_len();
        self.patch_jump_to(exit_from_pos, after);
        self.patch_jump_to(exit_from_neg, after);
        for b in breaks {
            self.patch_jump_to(b, after);
        }

        self.pop_scope();
        Ok(())
    }

    fn encode_expr(&mut self, program: &Program, expr: &Expr) -> Result<(), String> {
        match expr {
            Expr::Nil => emit_instr(self.code_mut(), Op::PushNil, 0, 0),
            Expr::Bool(true) => emit_instr(self.code_mut(), Op::PushTrue, 0, 0),
            Expr::Bool(false) => emit_instr(self.code_mut(), Op::PushFalse, 0, 0),
            Expr::Number(n) => {
                let k = self.intern_number(*n);
                emit_instr(self.code_mut(), Op::PushConst, k as i16, 0);
            }
            Expr::String(s) => {
                let k = self.intern_string(s);
                emit_instr(self.code_mut(), Op::PushConst, k as i16, 0);
            }
            Expr::Vararg => {
                emit_instr(self.code_mut(), Op::Vararg, -1, 0);
            }
            Expr::Name(name) => {
                if let Some(slot) = self.resolve_local(name) {
                    emit_instr(self.code_mut(), Op::LoadLocal, slot as i16, 0);
                } else if let Some(uv) = self.resolve_upvalue(name)? {
                    emit_instr(self.code_mut(), Op::LoadUpval, uv as i16, 0);
                } else {
                    let k = self.intern_string(name);
                    emit_instr(self.code_mut(), Op::LoadGlobal, k as i16, 0);
                }
            }
            Expr::Field { obj, name } => {
                self.encode_expr(program, obj)?;
                let k = self.intern_string(name);
                emit_instr(self.code_mut(), Op::GetField, k as i16, 0);
            }
            Expr::Index { obj, key } => {
                self.encode_expr(program, obj)?;
                self.encode_expr(program, key)?;
                emit_instr(self.code_mut(), Op::GetIndex, 0, 0);
            }
            Expr::BinOp { op, lhs, rhs } => self.encode_binop(program, *op, lhs, rhs)?,
            Expr::UnOp { op, rhs } => {
                self.encode_expr(program, rhs)?;
                let sub = match op {
                    UnOp::Neg => UnSubOp::Neg,
                    UnOp::Not => UnSubOp::Not,
                    UnOp::Len => UnSubOp::Len,
                };
                emit_instr(self.code_mut(), Op::UnOp, sub as i16, 0);
            }
            Expr::Call { func, args } => {
                self.encode_expr(program, func)?;
                for a in args {
                    self.encode_expr(program, a)?;
                }
                emit_instr(self.code_mut(), Op::Call, args.len() as i16, 1);
            }
            Expr::MethodCall { obj, method, args } => {
                // Push obj, MethodPrep -> pushes obj[method], pushes obj. Now
                // stack has [..., fn, self], then we push args and Call.
                self.encode_expr(program, obj)?;
                let k = self.intern_string(method);
                emit_instr(self.code_mut(), Op::MethodPrep, k as i16, 0);
                for a in args {
                    self.encode_expr(program, a)?;
                }
                // nargs = 1 (self) + provided args
                emit_instr(
                    self.code_mut(),
                    Op::Call,
                    (args.len() + 1) as i16,
                    1,
                );
            }
            Expr::Function(idx) => {
                // Encode the child proto first so it knows its upvalues, then
                // emit Closure + per-upvalue specs.
                if self.module.protos[*idx].code.is_empty() {
                    self.encode_proto(program, *idx)?;
                }
                let upvals = self.module.protos[*idx].upvalue_specs.clone();
                emit_instr(self.code_mut(), Op::Closure, *idx as i16, upvals.len() as i16);
                for (kind, index) in upvals {
                    emit_instr(self.code_mut(), Op::ClosureUpval, kind as i16, index as i16);
                }
            }
            Expr::Table(fields) => self.encode_table(program, fields)?,
        }
        Ok(())
    }

    fn encode_binop(
        &mut self,
        program: &Program,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Result<(), String> {
        // and/or short-circuit
        if matches!(op, BinOp::And | BinOp::Or) {
            self.encode_expr(program, lhs)?;
            let jump_op = if matches!(op, BinOp::And) {
                Op::JumpIfFalseKeep
            } else {
                Op::JumpIfTrueKeep
            };
            let j_pos = self.code_len();
            emit_instr(self.code_mut(), jump_op, 0, 0);
            // The "keep" jump preserves the LHS value if it short-circuits.
            // Otherwise we pop it and evaluate the RHS.
            emit_instr(self.code_mut(), Op::Pop, 1, 0);
            self.encode_expr(program, rhs)?;
            let after = self.code_len();
            self.patch_jump_to(j_pos, after);
            return Ok(());
        }
        self.encode_expr(program, lhs)?;
        self.encode_expr(program, rhs)?;
        let sub = match op {
            BinOp::Add => BinSubOp::Add,
            BinOp::Sub => BinSubOp::Sub,
            BinOp::Mul => BinSubOp::Mul,
            BinOp::Div => BinSubOp::Div,
            BinOp::Mod => BinSubOp::Mod,
            BinOp::Pow => BinSubOp::Pow,
            BinOp::Concat => BinSubOp::Concat,
            BinOp::Eq => BinSubOp::Eq,
            BinOp::Ne => BinSubOp::Ne,
            BinOp::Lt => BinSubOp::Lt,
            BinOp::Le => BinSubOp::Le,
            BinOp::Gt => BinSubOp::Gt,
            BinOp::Ge => BinSubOp::Ge,
            BinOp::FloorDiv => BinSubOp::FloorDiv,
            BinOp::And | BinOp::Or => unreachable!("handled above"),
        };
        emit_instr(self.code_mut(), Op::BinOp, sub as i16, 0);
        Ok(())
    }

    fn encode_table(&mut self, program: &Program, fields: &[TableField]) -> Result<(), String> {
        let array_hint = fields
            .iter()
            .filter(|f| matches!(f, TableField::Array(_)))
            .count();
        let hash_hint = fields.len() - array_hint;
        emit_instr(
            self.code_mut(),
            Op::NewTable,
            array_hint as i16,
            hash_hint as i16,
        );
        let mut array_idx: i16 = 0;
        for f in fields {
            match f {
                TableField::Array(e) => {
                    array_idx += 1;
                    self.encode_expr(program, e)?;
                    emit_instr(self.code_mut(), Op::SetListIndex, array_idx, 0);
                }
                TableField::Named { name, value } => {
                    self.encode_expr(program, value)?;
                    let k = self.intern_string(name);
                    emit_instr(self.code_mut(), Op::SetField, k as i16, 0);
                }
                TableField::Indexed { key, value } => {
                    self.encode_expr(program, key)?;
                    self.encode_expr(program, value)?;
                    emit_instr(self.code_mut(), Op::SetIndex, 0, 0);
                }
            }
        }
        Ok(())
    }

    // ---------- frame helpers ----------

    fn frame(&self) -> &Frame {
        self.frames.last().expect("no active frame")
    }

    fn frame_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("no active frame")
    }

    fn code_mut(&mut self) -> &mut Vec<u8> {
        &mut self.frame_mut().code
    }

    fn code_len(&self) -> usize {
        self.frame().code.len()
    }

    fn push_scope(&mut self) {
        let base = self.frame().next_slot;
        self.frame_mut().scopes.push(Scope {
            locals: Vec::new(),
            base_slot: base,
        });
    }

    fn pop_scope(&mut self) {
        let frame = self.frame_mut();
        if let Some(scope) = frame.scopes.pop() {
            frame.next_slot = scope.base_slot;
        }
    }

    fn alloc_local(&mut self, name: String) -> u16 {
        let frame = self.frame_mut();
        let slot = frame.next_slot;
        frame.next_slot += 1;
        if frame.next_slot > frame.max_slot {
            frame.max_slot = frame.next_slot;
        }
        let scope = frame
            .scopes
            .last_mut()
            .expect("alloc_local with no active scope");
        scope.locals.push(LocalBinding { name, slot });
        slot
    }

    fn alloc_anon_local(&mut self) -> u16 {
        self.alloc_local(String::new())
    }

    fn resolve_local(&self, name: &str) -> Option<u16> {
        let frame = self.frame();
        for scope in frame.scopes.iter().rev() {
            for b in scope.locals.iter().rev() {
                if !b.name.is_empty() && b.name == name {
                    return Some(b.slot);
                }
            }
        }
        None
    }

    /// Walk parent frames to find a local with this name. If found, ensure
    /// every intermediate frame has a corresponding upvalue entry and return
    /// the upvalue index in the current frame.
    fn resolve_upvalue(&mut self, name: &str) -> Result<Option<u16>, String> {
        if self.frames.len() < 2 {
            return Ok(None);
        }
        // First check if we already have this upvalue.
        if let Some(idx) = self
            .frame()
            .upvalues
            .iter()
            .position(|u| u.name == name)
        {
            return Ok(Some(idx as u16));
        }
        // Recursively resolve in parent.
        let current_idx = self.frames.len() - 1;
        let parent_idx = current_idx - 1;

        // Temporarily pop current frame so parent becomes top.
        let saved = self.frames.pop().unwrap();
        let found = self.resolve_in_frame(parent_idx, name)?;
        self.frames.push(saved);

        let (kind, parent_slot_or_uv) = match found {
            Some(spec) => spec,
            None => return Ok(None),
        };

        let frame = self.frame_mut();
        let idx = frame.upvalues.len() as u16;
        frame.upvalues.push(UpvalSpec {
            kind,
            index: parent_slot_or_uv,
            name: name.to_string(),
        });
        Ok(Some(idx))
    }

    /// In the frame at `frame_idx`, look for `name` as a local or upvalue,
    /// recursively materializing intermediate upvalues. Returns the (kind,
    /// index) pair to use as an upvalue spec in the frame ABOVE `frame_idx`.
    fn resolve_in_frame(
        &mut self,
        frame_idx: usize,
        name: &str,
    ) -> Result<Option<(u8, u16)>, String> {
        // Check locals of frame_idx
        let frame = &self.frames[frame_idx];
        for scope in frame.scopes.iter().rev() {
            for b in scope.locals.iter().rev() {
                if !b.name.is_empty() && b.name == name {
                    return Ok(Some((0, b.slot)));
                }
            }
        }
        // Check existing upvalues
        if let Some(uv_idx) = frame.upvalues.iter().position(|u| u.name == name) {
            return Ok(Some((1, uv_idx as u16)));
        }
        // Recurse to parent
        if frame_idx == 0 {
            return Ok(None);
        }
        let parent_resolved = self.resolve_in_frame(frame_idx - 1, name)?;
        if let Some((kind, idx)) = parent_resolved {
            let frame = &mut self.frames[frame_idx];
            let new_idx = frame.upvalues.len() as u16;
            frame.upvalues.push(UpvalSpec {
                kind,
                index: idx,
                name: name.to_string(),
            });
            Ok(Some((1, new_idx)))
        } else {
            Ok(None)
        }
    }

    // ---------- const pool ----------

    fn intern_number(&mut self, n: f64) -> u16 {
        let c = Const::Number(n);
        self.intern(c)
    }

    fn intern_string(&mut self, s: &str) -> u16 {
        let c = Const::String(s.as_bytes().to_vec());
        self.intern(c)
    }

    fn intern(&mut self, c: Const) -> u16 {
        let key = c.as_key();
        if let Some(&i) = self.const_pool.get(&key) {
            return i;
        }
        let i = self.module.constants.len() as u16;
        self.module.constants.push(c);
        self.const_pool.insert(key, i);
        i
    }

    // ---------- jump patching ----------

    fn patch_jump_to(&mut self, at: usize, target: usize) {
        let next_pc = (at + INSTR_WIDTH) as i32;
        let delta = target as i32 - next_pc;
        let delta_instrs = delta / INSTR_WIDTH as i32;
        if delta_instrs > i16::MAX as i32 || delta_instrs < i16::MIN as i32 {
            // Out of range — error here would surface bugs early. For Phase 1
            // we panic loudly; Phase 6 will report this gracefully.
            panic!(
                "jump out of i16 range: delta_instrs={delta_instrs} at={at} target={target}"
            );
        }
        patch_a(&mut self.code_mut(), at, delta_instrs as i16);
    }
}
