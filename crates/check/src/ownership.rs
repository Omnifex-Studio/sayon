//! Ownership checking: moves, initialization, borrow conflicts, and inference
//! of parameter modes.
//!
//! The analysis walks each function's HIR with a flow-sensitive record of
//! which locals hold a value. It decides, for every read of a local, whether
//! the value is copied, moved, or borrowed, and rejects programs that use a
//! value after transferring it, change a collection while a loop reads it, or
//! move or change data they only borrow.

use crate::hir::*;
use crate::types::Type;
use sayon_syntax::ast::ParamMode;
use sayon_syntax::{Diagnostic, Span};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
enum MoveReason {
    Assign(String),
    Call(String),
    Return,
    Element,
    Structure,
    Outcome,
    Explicit,
}

#[derive(Clone, Debug, PartialEq)]
struct MoveSite {
    span: Span,
    reason: MoveReason,
    /// The move happened on an earlier iteration of an enclosing loop.
    earlier_iteration: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Init,
    Moved(MoveSite),
    MaybeMoved(MoveSite),
}

/// `None` marks unreachable code.
type State = Option<HashMap<LocalId, Status>>;

fn join(a: &State, b: &State) -> State {
    match (a, b) {
        (None, other) | (other, None) => other.clone(),
        (Some(x), Some(y)) => {
            let mut out = HashMap::new();
            for (local, left) in x {
                let merged = match (left, y.get(local)) {
                    (Status::Init, Some(Status::Init)) => Status::Init,
                    (Status::Init, None) => continue,
                    (Status::Init, Some(Status::Moved(site) | Status::MaybeMoved(site)))
                    | (Status::Moved(site) | Status::MaybeMoved(site), _) => {
                        Status::MaybeMoved(site.clone())
                    }
                };
                out.insert(*local, merged);
            }
            for (local, right) in y {
                if !x.contains_key(local)
                    && let Status::Moved(site) | Status::MaybeMoved(site) = right
                {
                    out.insert(*local, Status::MaybeMoved(site.clone()));
                }
            }
            Some(out)
        }
    }
}

/// Joins the loop entry state with the state flowing back from the body,
/// marking moves that arrive over the back edge.
fn join_back_edge(entry: &State, back: &State) -> State {
    let marked = back.as_ref().map(|map| {
        map.iter()
            .map(|(local, status)| {
                let status = match status {
                    Status::Moved(site) | Status::MaybeMoved(site) => {
                        let mut site = site.clone();
                        site.earlier_iteration = true;
                        Status::Moved(site)
                    }
                    Status::Init => Status::Init,
                };
                (*local, status)
            })
            .collect()
    });
    let joined = join(entry, &marked);
    // Keep the original sites for moves that already existed on entry.
    match (joined, entry) {
        (Some(mut joined), Some(entry)) => {
            for (local, status) in entry {
                if let Status::Moved(site) | Status::MaybeMoved(site) = status {
                    joined.insert(*local, Status::MaybeMoved(site.clone()));
                }
            }
            Some(joined)
        }
        (joined, _) => joined,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctx {
    Consume,
    Read,
    Update,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    Move,
    Read,
    Update,
}

/// A local read in place by a statement that runs other code meanwhile: a
/// loop over a collection, or a match that binds parts of its value.
struct LoopBorrow {
    local: LocalId,
    span: Span,
    /// "loop" or "match".
    by: &'static str,
}

#[derive(Default)]
struct LoopFrame {
    breaks: Vec<State>,
    continues: Vec<State>,
}

struct Analyzer<'p> {
    modes: &'p HashMap<FuncId, Vec<ParamMode>>,
    names: &'p HashMap<FuncId, String>,
    constant_names: &'p [String],
    locals: &'p mut Vec<Local>,
    /// Parameter modes of the function being analyzed, by local.
    params: HashMap<LocalId, (ParamMode, bool)>,
    report: bool,
    diagnostics: Vec<Diagnostic>,
    reported: HashSet<(&'static str, Span)>,
    loops: Vec<LoopFrame>,
    loop_borrows: Vec<LoopBorrow>,
    regions: Vec<HashSet<LocalId>>,
    reason: MoveReason,
}

impl<'p> Analyzer<'p> {
    fn emit(&mut self, code: &'static str, span: Span, build: impl FnOnce() -> Diagnostic) {
        if self.report && self.reported.insert((code, span)) {
            self.diagnostics.push(build());
        }
    }

    fn local(&self, local: LocalId) -> &Local {
        &self.locals[local.0 as usize]
    }

    fn describe_value(ty: &Type) -> &'static str {
        match ty {
            Type::Array(_) => "the array",
            Type::Dict(..) => "the dictionary",
            Type::String => "the string",
            Type::Error => "the error",
            _ => "the value",
        }
    }

    /// Upgrades an inferred parameter mode, or reports a violation of an explicit one.
    fn require_param_mode(&mut self, local: LocalId, needed: ParamMode, span: Span) -> bool {
        let Some((mode, explicit)) = self.params.get(&local).copied() else {
            return true;
        };
        let rank = |m: ParamMode| match m {
            ParamMode::Read => 0,
            ParamMode::Update => 1,
            ParamMode::Own => 2,
        };
        if rank(mode) >= rank(needed) {
            return true;
        }
        if !explicit {
            self.params.insert(local, (needed, false));
            return true;
        }
        let name = self.local(local).name.clone();
        let ty = self.local(local).ty.clone();
        let declared = self.local(local).span;
        let (code, message, help) = match needed {
            ParamMode::Own => (
                "E_MOVE_BORROWED",
                format!(
                    "cannot transfer `{name}`: the parameter only borrows {}",
                    Self::describe_value(&ty)
                ),
                format!("declare it `own {name}: {ty}` to take ownership, or use clone({name})"),
            ),
            _ => (
                "E_MUTATE_BORROWED",
                format!("cannot change `{name}`: the parameter is a read-only borrow"),
                format!("declare it `update {name}: {ty}` to allow changes the caller will see"),
            ),
        };
        self.emit(code, span, || {
            Diagnostic::error(code, message)
                .primary(span, "")
                .secondary(declared, "parameter declared here")
                .help(help)
        });
        false
    }

    fn check_initialized(&mut self, local: LocalId, span: Span, state: &State) -> bool {
        let Some(map) = state else { return true };
        let (site, maybe) = match map.get(&local) {
            Some(Status::Moved(site)) => (site.clone(), false),
            Some(Status::MaybeMoved(site)) => (site.clone(), true),
            _ => return true,
        };
        let name = self.local(local).name.clone();
        let value = Self::describe_value(&self.local(local).ty);
        let destination = match &site.reason {
            MoveReason::Assign(to) => format!(" to `{to}`"),
            MoveReason::Call(callee) => format!(" to `{callee}`"),
            MoveReason::Return => " by `return`".to_string(),
            MoveReason::Element => " into a collection".to_string(),
            MoveReason::Structure => " into a structure".to_string(),
            MoveReason::Outcome => " into an Outcome".to_string(),
            MoveReason::Explicit => String::new(),
        };
        let message = if site.earlier_iteration {
            format!("`{name}` was transferred{destination} in an earlier iteration of the loop")
        } else if maybe {
            format!("`{name}` may have been transferred{destination}")
        } else {
            format!("`{name}` was transferred{destination}")
        };
        let help = match &site.reason {
            MoveReason::Assign(to) => format!(
                "use `{to}` here, or write `{to} = clone({name})` when you need two independent copies"
            ),
            MoveReason::Call(callee) => format!(
                "`{callee}` takes ownership; pass clone({name}) if you still need `{name}` afterwards"
            ),
            _ => format!("write clone({name}) at the transfer if you need to keep using `{name}`"),
        };
        let move_label = if maybe && !site.earlier_iteration {
            "transferred here on some paths"
        } else {
            "ownership transferred here"
        };
        self.emit("E_MOVED_VALUE", span, || {
            let diagnostic = Diagnostic::error("E_MOVED_VALUE", message);
            let diagnostic = if site.span == span {
                diagnostic.primary(
                    span,
                    format!("transferred here, so the next iteration no longer owns {value}"),
                )
            } else {
                diagnostic
                    .secondary(site.span, move_label)
                    .primary(span, format!("`{name}` no longer owns {value}"))
            };
            diagnostic.help(help)
        });
        false
    }

    fn loop_conflict(&mut self, local: LocalId, span: Span, action: &str) -> bool {
        let Some(borrow) = self.loop_borrows.iter().find(|b| b.local == local) else {
            return true;
        };
        let loop_span = borrow.span;
        let by = borrow.by;
        let name = self.local(local).name.clone();
        let (secondary, help) = if by == "loop" {
            (
                format!("the loop reads `{name}` here until it finishes"),
                "collect the changes in another collection and apply them after the loop",
            )
        } else {
            (
                format!("the match reads `{name}` here while its case runs"),
                "make the change after the match, or bind a copy with clone(...)",
            )
        };
        self.emit("E_BORROW_CONFLICT", span, || {
            Diagnostic::error(
                "E_BORROW_CONFLICT",
                format!("cannot {action} `{name}` while the {by} is reading it"),
            )
            .primary(span, "")
            .secondary(loop_span, secondary)
            .help(help)
        });
        false
    }

    fn mark_moved(&mut self, local: LocalId, span: Span, state: &mut State) {
        if let Some(map) = state {
            let site = MoveSite {
                span,
                reason: self.reason.clone(),
                earlier_iteration: false,
            };
            map.insert(local, Status::Moved(site));
        }
        for region in &mut self.regions {
            region.insert(local);
        }
    }

    /// Classifies and checks one read of a local.
    fn use_local(
        &mut self,
        local: LocalId,
        usage: &mut Usage,
        copy: bool,
        ctx: Ctx,
        span: Span,
        state: &mut State,
    ) {
        let explicit_move = *usage == Usage::Move;
        let decided = if copy && ctx != Ctx::Update {
            Usage::Copy
        } else if explicit_move || ctx == Ctx::Consume {
            Usage::Move
        } else if ctx == Ctx::Update {
            Usage::BorrowUpdate
        } else {
            Usage::Borrow
        };
        *usage = if explicit_move && !copy {
            Usage::Move
        } else {
            decided
        };

        if !self.check_initialized(local, span, state) {
            return;
        }
        let info = self.local(local).clone();
        match decided {
            Usage::Move => {
                if info.kind == LocalKind::LoopVariable && info.borrowed {
                    let name = info.name.clone();
                    self.emit("E_MOVE_BORROWED", span, || {
                        Diagnostic::error(
                            "E_MOVE_BORROWED",
                            format!(
                                "cannot transfer `{name}`: it borrows an element during the loop"
                            ),
                        )
                        .primary(span, "")
                        .help(format!("use clone({name}) to keep a copy of the element"))
                    });
                    return;
                }
                if info.kind == LocalKind::CaseBinding && info.borrowed {
                    let name = info.name.clone();
                    self.emit("E_MOVE_BORROWED", span, || {
                        Diagnostic::error(
                            "E_MOVE_BORROWED",
                            format!(
                                "cannot transfer `{name}`: it borrows part of the matched value"
                            ),
                        )
                        .primary(span, "")
                        .secondary(info.span, "bound here")
                        .help(format!(
                            "use clone({name}) for a copy, or match on move(...) to take the value apart (in a case without an `if` guard)"
                        ))
                    });
                    return;
                }
                if !self.require_param_mode(local, ParamMode::Own, span) {
                    return;
                }
                if !self.loop_conflict(local, span, "transfer") {
                    return;
                }
                self.mark_moved(local, span, state);
            }
            // An `update` parameter receives a reference even to a Copy value.
            Usage::BorrowUpdate if copy => self.check_update_copy(local, span),
            Usage::BorrowUpdate => self.check_update(local, span),
            _ => {}
        }
    }

    fn check_update(&mut self, local: LocalId, span: Span) {
        let info = self.local(local).clone();
        match info.kind {
            LocalKind::Constant => {
                let name = info.name.clone();
                self.emit("E_MUTATE_CONSTANT", span, || {
                    Diagnostic::error(
                        "E_MUTATE_CONSTANT",
                        format!("`{name}` is a constant and cannot be changed"),
                    )
                    .primary(span, "")
                    .secondary(info.span, "declared as a constant here")
                    .help(format!(
                        "declare it with `variable {name} = ...` to allow changes"
                    ))
                });
                return;
            }
            LocalKind::LoopVariable if info.borrowed => {
                let name = info.name.clone();
                self.emit("E_MUTATE_BORROWED", span, || {
                    Diagnostic::error(
                        "E_MUTATE_BORROWED",
                        format!("cannot change `{name}`: it borrows an element during the loop"),
                    )
                    .primary(span, "")
                    .help("loop over indexes with range(length(...)) to change elements in place")
                });
                return;
            }
            LocalKind::CaseBinding if info.borrowed => {
                let name = info.name.clone();
                self.emit("E_MUTATE_BORROWED", span, || {
                    Diagnostic::error(
                        "E_MUTATE_BORROWED",
                        format!("cannot change `{name}`: it borrows part of the matched value"),
                    )
                    .primary(span, "")
                    .secondary(info.span, "bound here")
                    .help("change the matched value itself after the match")
                });
                return;
            }
            _ => {}
        }
        if !self.require_param_mode(local, ParamMode::Update, span) {
            return;
        }
        self.loop_conflict(local, span, "change");
    }

    /// The local an expression accesses in place, if any.
    fn root(expr: &Expr) -> Option<LocalId> {
        match &expr.kind {
            ExprKind::Local { local, .. } => Some(*local),
            ExprKind::Narrow(inner)
            | ExprKind::Inject(inner)
            | ExprKind::VariantPayload { value: inner, .. } => Self::root(inner),
            ExprKind::Index { target, .. } | ExprKind::Field { value: target, .. } => {
                Self::root(target)
            }
            _ => None,
        }
    }

    /// Whether the expression reads storage that outlives it (a variable,
    /// constant, or part of one), rather than a temporary value.
    fn is_place(expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Local { .. } | ExprKind::Constant(_) => true,
            ExprKind::Narrow(inner)
            | ExprKind::Inject(inner)
            | ExprKind::VariantPayload { value: inner, .. } => Self::is_place(inner),
            ExprKind::Index { target, .. } | ExprKind::Field { value: target, .. } => {
                Self::is_place(target)
            }
            _ => false,
        }
    }

    fn expr(&mut self, expr: &mut Expr, ctx: Ctx, state: &mut State) {
        let span = expr.span;
        let ty = expr.ty.clone();
        match &mut expr.kind {
            ExprKind::Local { local, usage } => {
                let local = *local;
                let copy = self.local(local).ty.is_copy();
                self.use_local(local, usage, copy, ctx, span, state);
            }
            // Using a narrowed value uses the local that holds it; taking
            // the value leaves the whole local spent.
            ExprKind::Narrow(inner) | ExprKind::VariantPayload { value: inner, .. } => {
                match &mut inner.kind {
                    ExprKind::Local { local, usage } => {
                        let local = *local;
                        self.use_local(local, usage, ty.is_copy(), ctx, span, state);
                    }
                    _ => self.expr(inner, ctx, state),
                }
            }
            ExprKind::TryExpr(inner) => {
                let saved = std::mem::replace(&mut self.reason, MoveReason::Outcome);
                self.expr(inner, Ctx::Consume, state);
                self.reason = saved;
            }
            ExprKind::IsVariant { value, .. } => self.expr(value, Ctx::Read, state),
            ExprKind::Constant(id) => {
                if ctx == Ctx::Update {
                    let name = self.constant_names[id.0 as usize].clone();
                    self.emit("E_MUTATE_CONSTANT", span, || {
                        Diagnostic::error(
                            "E_MUTATE_CONSTANT",
                            format!("the constant `{name}` cannot be changed"),
                        )
                        .primary(span, "")
                        .help(format!("copy it first: `items = clone({name})`"))
                    });
                } else if ctx == Ctx::Consume && !ty.is_copy() {
                    let name = self.constant_names[id.0 as usize].clone();
                    self.emit("E_MOVE_CONSTANT", span, || {
                        Diagnostic::error(
                            "E_MOVE_CONSTANT",
                            format!("cannot transfer the constant `{name}`"),
                        )
                        .primary(span, "constants keep their value for the whole program")
                        .help(format!("use clone({name}) for an independent copy"))
                    });
                }
            }
            ExprKind::Call { function, args, .. } => {
                let modes = self.modes.get(function).cloned().unwrap_or_default();
                let callee = self.names.get(function).cloned().unwrap_or_default();
                for (index, arg) in args.iter_mut().enumerate() {
                    arg.mode = modes.get(index).copied().unwrap_or(ParamMode::Read);
                }
                self.args(args, &callee, state);
            }
            ExprKind::Builtin { builtin, args } => {
                let name = builtin.name().to_string();
                self.args(args, &name, state);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, Ctx::Read, state);
                self.expr(rhs, Ctx::Read, state);
            }
            ExprKind::Unary { operand, .. } => self.expr(operand, Ctx::Read, state),
            ExprKind::Array(items) => {
                let saved = std::mem::replace(&mut self.reason, MoveReason::Element);
                for item in items {
                    self.expr(item, Ctx::Consume, state);
                }
                self.reason = saved;
            }
            ExprKind::Dict(entries) => {
                let saved = std::mem::replace(&mut self.reason, MoveReason::Element);
                for (key, value) in entries {
                    self.expr(key, Ctx::Read, state);
                    self.expr(value, Ctx::Consume, state);
                }
                self.reason = saved;
            }
            ExprKind::Record(fields) => {
                let saved = std::mem::replace(&mut self.reason, MoveReason::Structure);
                for (_, value) in fields {
                    self.expr(value, Ctx::Consume, state);
                }
                self.reason = saved;
            }
            ExprKind::Variant { args, .. } => {
                let saved = std::mem::replace(&mut self.reason, MoveReason::Structure);
                for value in args {
                    self.expr(value, Ctx::Consume, state);
                }
                self.reason = saved;
            }
            ExprKind::Field { value, name } => {
                if ctx == Ctx::Consume && !ty.is_copy() && Self::is_place(value) {
                    // Taking one field would leave the structure incomplete.
                    let field_value = Analyzer::describe_value(&ty);
                    let owner = Self::root(value)
                        .map(|local| format!("`{}`", self.local(local).name))
                        .unwrap_or_else(|| "a constant".to_string());
                    let name = name.clone();
                    self.emit("E_MOVE_OUT_OF_FIELD", span, || {
                        Diagnostic::error(
                            "E_MOVE_OUT_OF_FIELD",
                            format!(
                                "cannot transfer {field_value} out of the field `{name}` of {owner}"
                            ),
                        )
                        .primary(span, "the field stays owned by its structure")
                        .help("use clone(...) for a copy, or transfer the whole structure")
                    });
                    self.expr(value, Ctx::Read, state);
                } else {
                    // A field of a temporary can be taken; the rest is destroyed.
                    let inner_ctx = match ctx {
                        Ctx::Consume if !Self::is_place(value) => Ctx::Consume,
                        Ctx::Update => Ctx::Update,
                        _ => Ctx::Read,
                    };
                    self.expr(value, inner_ctx, state);
                }
            }
            ExprKind::Index { target, index } => {
                self.expr(index, Ctx::Read, state);
                if ctx == Ctx::Consume && !ty.is_copy() {
                    let element = Analyzer::describe_value(&ty);
                    self.emit("E_MOVE_OUT_OF_ELEMENT", span, || {
                        Diagnostic::error(
                            "E_MOVE_OUT_OF_ELEMENT",
                            format!("cannot transfer {element} out of a collection element"),
                        )
                        .primary(span, "this element stays owned by its collection")
                        .help("use clone(...) for a copy, or pop(...) to remove the last element")
                    });
                    self.expr(target, Ctx::Read, state);
                } else {
                    let target_ctx = if ctx == Ctx::Update {
                        Ctx::Update
                    } else {
                        Ctx::Read
                    };
                    self.expr(target, target_ctx, state);
                }
            }
            ExprKind::Interpolate(segments) => {
                for segment in segments {
                    if let Segment::Value(value) = segment {
                        self.expr(value, Ctx::Read, state);
                    }
                }
            }
            ExprKind::Inject(inner) => {
                let place =
                    Self::root(inner).is_some() || matches!(inner.kind, ExprKind::Constant(_));
                if ctx != Ctx::Consume && place && !inner.ty.is_copy() {
                    let inner_ty = inner.ty.clone();
                    self.emit("E_BORROWED_UNION", span, || {
                        Diagnostic::error(
                            "E_BORROWED_UNION",
                            format!("this {inner_ty} would have to be copied to become a {ty}"),
                        )
                        .primary(span, "")
                        .help("pass clone(...) for a copy, or move(...) to hand the value over")
                    });
                    self.expr(inner, Ctx::Read, state);
                } else {
                    self.expr(inner, ctx, state);
                }
            }
            ExprKind::IsMember { value, .. }
            | ExprKind::UnionTypeName(value)
            | ExprKind::ErrorField { value, .. } => self.expr(value, Ctx::Read, state),
            ExprKind::Integer(_)
            | ExprKind::Float(_)
            | ExprKind::Boolean(_)
            | ExprKind::String(_)
            | ExprKind::Nothing => {}
        }
    }

    fn args(&mut self, args: &mut [Arg], callee: &str, state: &mut State) {
        let saved = std::mem::replace(&mut self.reason, MoveReason::Call(callee.to_string()));
        let mut accesses: Vec<(LocalId, Access, Span)> = Vec::new();
        for arg in args.iter_mut() {
            let ctx = match arg.mode {
                ParamMode::Own => Ctx::Consume,
                ParamMode::Update => Ctx::Update,
                ParamMode::Read => Ctx::Read,
            };
            self.expr(&mut arg.expr, ctx, state);
            if let Some(root) = Self::root(&arg.expr) {
                let access = match ctx {
                    _ if arg.expr.ty.is_copy() && ctx != Ctx::Update => None,
                    Ctx::Consume => Some(Access::Move),
                    Ctx::Update => Some(Access::Update),
                    Ctx::Read => Some(Access::Read),
                };
                if let Some(access) = access {
                    accesses.push((root, access, arg.expr.span));
                }
            }
        }
        self.reason = saved;
        for (i, (local, first, first_span)) in accesses.iter().enumerate() {
            for (other_local, second, second_span) in &accesses[i + 1..] {
                if local != other_local || (*first == Access::Read && *second == Access::Read) {
                    continue;
                }
                let name = self.local(*local).name.clone();
                let (first_span, second_span) = (*first_span, *second_span);
                let describe = |access: Access| match access {
                    Access::Move => "transferred",
                    Access::Update => "borrowed for changes",
                    Access::Read => "borrowed for reading",
                };
                let (a, b) = (describe(*first), describe(*second));
                let callee = callee.to_string();
                self.emit("E_BORROW_CONFLICT", second_span, || {
                    Diagnostic::error(
                        "E_BORROW_CONFLICT",
                        format!("`{name}` is used twice in one call to `{callee}`"),
                    )
                    .secondary(first_span, format!("{a} here"))
                    .primary(second_span, format!("and {b} here"))
                    .help(format!(
                        "`{callee}` needs exclusive access; pass a clone or split the call"
                    ))
                });
            }
        }
    }

    fn write_place(&mut self, place: &mut Place, state: &mut State, span: Span) {
        match place {
            Place::Local(local) => {
                let local = *local;
                let ty = self.local(local).ty.clone();
                if !ty.is_copy() && !self.require_param_mode(local, ParamMode::Update, span) {
                    return;
                }
                if !self.loop_conflict(local, span, "replace") {
                    return;
                }
                if let Some(map) = state {
                    map.insert(local, Status::Init);
                }
            }
            Place::Index { target, index, .. } => {
                self.expr(index, Ctx::Read, state);
                self.update_place(target, state, span);
            }
            Place::Field { target, .. } | Place::Narrow { target, .. } => {
                self.update_place(target, state, span)
            }
        }
    }

    /// The place is changed in place (an element or field assignment, or a
    /// compound update).
    fn update_place(&mut self, place: &mut Place, state: &mut State, span: Span) {
        match place {
            Place::Local(local) => {
                let local = *local;
                if !self.check_initialized(local, span, state) {
                    return;
                }
                if self.local(local).ty.is_copy() {
                    // A Copy value (such as a structure of numbers) belongs to
                    // this function even when it arrived as a parameter.
                    self.check_update_copy(local, span);
                } else {
                    self.check_update(local, span);
                }
            }
            Place::Index { target, index, .. } => {
                self.expr(index, Ctx::Read, state);
                self.update_place(target, state, span);
            }
            Place::Field { target, .. } | Place::Narrow { target, .. } => {
                self.update_place(target, state, span)
            }
        }
    }

    fn check_update_copy(&mut self, local: LocalId, span: Span) {
        let info = self.local(local).clone();
        if info.kind == LocalKind::Constant {
            let name = info.name.clone();
            self.emit("E_MUTATE_CONSTANT", span, || {
                Diagnostic::error(
                    "E_MUTATE_CONSTANT",
                    format!("`{name}` is a constant and cannot be changed"),
                )
                .primary(span, "")
                .secondary(info.span, "declared as a constant here")
                .help(format!(
                    "declare it with `variable {name} = ...` to allow changes"
                ))
            });
            return;
        }
        if matches!(info.kind, LocalKind::LoopVariable | LocalKind::CaseBinding) {
            let name = info.name.clone();
            let what = if info.kind == LocalKind::LoopVariable {
                "the loop variable"
            } else {
                "the case binding"
            };
            self.emit("E_ASSIGN_CONSTANT", span, || {
                Diagnostic::error(
                    "E_ASSIGN_CONSTANT",
                    format!("{what} `{name}` cannot be changed"),
                )
                .primary(span, "")
                .help(format!("copy it into a variable first: `copy = {name}`"))
            });
            return;
        }
        self.loop_conflict(local, span, "change");
    }

    fn place_name(&self, place: &Place) -> String {
        self.local(place.root()).name.clone()
    }

    fn block(&mut self, block: &mut Block, state: &mut State) {
        for stmt in &mut block.stmts {
            if state.is_none() {
                break;
            }
            self.stmt(stmt, state);
        }
    }

    fn stmt(&mut self, stmt: &mut Stmt, state: &mut State) {
        let span = stmt.span;
        match &mut stmt.kind {
            StmtKind::Let { local, value } => {
                let name = self.local(*local).name.clone();
                let saved = std::mem::replace(&mut self.reason, MoveReason::Assign(name));
                self.expr(value, Ctx::Consume, state);
                self.reason = saved;
                if let Some(map) = state {
                    map.insert(*local, Status::Init);
                }
            }
            StmtKind::Assign { place, value } => {
                let name = self.place_name(place);
                let saved = std::mem::replace(&mut self.reason, MoveReason::Assign(name));
                self.expr(value, Ctx::Consume, state);
                self.reason = saved;
                self.write_place(place, state, span);
            }
            StmtKind::CompoundAssign { place, value, .. } => {
                self.expr(value, Ctx::Read, state);
                match place {
                    Place::Local(local) => {
                        let local = *local;
                        if self.check_initialized(local, span, state) {
                            let ty = self.local(local).ty.clone();
                            if !ty.is_copy() {
                                self.check_update(local, span);
                            } else {
                                self.loop_conflict(local, span, "change");
                            }
                        }
                    }
                    _ => self.update_place(place, state, span),
                }
            }
            StmtKind::Expr(value) => {
                let diverges = matches!(
                    value.kind,
                    ExprKind::Builtin {
                        builtin: Builtin::Error | Builtin::Raise,
                        ..
                    }
                );
                self.expr(value, Ctx::Read, state);
                if diverges {
                    *state = None;
                }
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition, Ctx::Read, state);
                let mut then_state = state.clone();
                self.block(then_block, &mut then_state);
                let mut else_state = state.clone();
                if let Some(block) = else_block {
                    self.block(block, &mut else_state);
                }
                *state = join(&then_state, &else_state);
            }
            StmtKind::While { condition, body } => {
                self.run_loop(state, None, |this, head| {
                    this.expr(condition, Ctx::Read, head);
                    let exit = head.clone();
                    this.block(body, head);
                    exit
                });
            }
            StmtKind::For {
                bindings,
                iterable,
                body,
            } => {
                self.expr(iterable, Ctx::Read, state);
                let iterable_ty = iterable.ty.clone();
                let collection = matches!(iterable_ty, Type::Array(_) | Type::Dict(..));
                if collection {
                    let element_types: Vec<Type> = match &iterable_ty {
                        Type::Array(element) => vec![(**element).clone()],
                        Type::Dict(key, value) => vec![(**key).clone(), (**value).clone()],
                        _ => Vec::new(),
                    };
                    for (binding, element) in bindings.iter().zip(element_types) {
                        self.locals[binding.0 as usize].borrowed = !element.is_copy();
                    }
                }
                let borrow = if collection {
                    Self::root(iterable)
                } else {
                    None
                };
                if let Some(local) = borrow {
                    self.loop_borrows.push(LoopBorrow {
                        local,
                        span: iterable.span,
                        by: "loop",
                    });
                }
                let bindings = bindings.clone();
                self.run_loop(state, Some(&bindings), |this, head| {
                    let exit = head.clone();
                    this.block(body, head);
                    exit
                });
                if borrow.is_some() {
                    self.loop_borrows.pop();
                }
            }
            StmtKind::Break => {
                if let Some(frame) = self.loops.last_mut() {
                    frame.breaks.push(state.clone());
                }
                *state = None;
            }
            StmtKind::Continue => {
                if let Some(frame) = self.loops.last_mut() {
                    frame.continues.push(state.clone());
                }
                *state = None;
            }
            StmtKind::Return(value) => {
                if let Some(value) = value {
                    let saved = std::mem::replace(&mut self.reason, MoveReason::Return);
                    self.expr(value, Ctx::Consume, state);
                    self.reason = saved;
                }
                *state = None;
            }
            StmtKind::Try {
                body,
                catch,
                finally,
            } => {
                let before = state.clone();
                self.regions.push(HashSet::new());
                let mut body_state = before.clone();
                self.block(body, &mut body_state);
                let moved_in_body = self.regions.pop().unwrap_or_default();
                for region in &mut self.regions {
                    region.extend(moved_in_body.iter().copied());
                }
                // Any statement of the body may have failed, so moves made in
                // the body are uncertain on entry to the handler.
                let failure_state = before.map(|mut map| {
                    for local in &moved_in_body {
                        if let Some(site) = body_state.as_ref().and_then(|m| match m.get(local) {
                            Some(Status::Moved(site) | Status::MaybeMoved(site)) => {
                                Some(site.clone())
                            }
                            _ => None,
                        }) {
                            map.insert(*local, Status::MaybeMoved(site));
                        }
                    }
                    map
                });
                let after_catch = match catch {
                    Some((local, handler)) => {
                        let mut handler_state = failure_state.clone();
                        if let Some(map) = &mut handler_state {
                            map.insert(*local, Status::Init);
                        }
                        self.block(handler, &mut handler_state);
                        handler_state
                    }
                    None => None,
                };
                let normal = join(&body_state, &after_catch);
                match finally {
                    Some(block) => {
                        let mut on_failure = join(&normal, &failure_state);
                        self.block(block, &mut on_failure);
                        let mut on_success = normal.clone();
                        self.block(block, &mut on_success);
                        *state = if normal.is_some() { on_success } else { None };
                    }
                    None => *state = normal,
                }
            }
            StmtKind::Block(block) => self.block(block, state),
            StmtKind::Match {
                scrutinee,
                owned,
                arms,
            } => self.match_stmt(scrutinee, *owned, arms, state),
        }
    }

    /// A match reads its value in place, binding parts by reference, unless
    /// it owns the value (a temporary or `move(...)`): then the bindings of
    /// arms without a guard own their parts. Copy values are matched on a
    /// copy. While a guard runs, and while an arm with borrowing bindings
    /// runs, the matched variable cannot change.
    fn match_stmt(
        &mut self,
        scrutinee: &mut Expr,
        owned: bool,
        arms: &mut [MatchArm],
        state: &mut State,
    ) {
        let copy = scrutinee.ty.is_copy();
        let consuming = owned && !copy;
        let saved = std::mem::replace(&mut self.reason, MoveReason::Explicit);
        let ctx = if owned { Ctx::Consume } else { Ctx::Read };
        self.expr(scrutinee, ctx, state);
        self.reason = saved;
        let root = if owned || copy {
            None
        } else {
            Self::root(scrutinee)
        };
        let mut after: State = None;
        for arm in arms {
            let mut arm_state = state.clone();
            let mut bindings = Vec::new();
            arm.pattern.bindings(&mut bindings);
            let guarded = arm.guard.is_some();
            let mut borrows = false;
            for binding in bindings {
                let borrowed = !self.local(binding).ty.is_copy() && (!consuming || guarded);
                self.locals[binding.0 as usize].borrowed = borrowed;
                borrows |= borrowed;
                if let Some(map) = &mut arm_state {
                    map.insert(binding, Status::Init);
                }
            }
            if let Some(local) = root {
                self.loop_borrows.push(LoopBorrow {
                    local,
                    span: scrutinee.span,
                    by: "match",
                });
            }
            if let Some(guard) = &mut arm.guard {
                self.expr(guard, Ctx::Read, &mut arm_state);
            }
            let borrowed_body = root.is_some() && borrows;
            if root.is_some() && !borrowed_body {
                self.loop_borrows.pop();
            }
            self.block(&mut arm.body, &mut arm_state);
            if borrowed_body {
                self.loop_borrows.pop();
            }
            after = join(&after, &arm_state);
        }
        *state = after;
    }

    /// Runs a loop body to a fixed point. `body` analyses one iteration from
    /// the loop head and returns the state on which the loop exits normally.
    fn run_loop(
        &mut self,
        state: &mut State,
        bindings: Option<&[LocalId]>,
        mut body: impl FnMut(&mut Self, &mut State) -> State,
    ) {
        let entry = state.clone();
        let mut head = entry.clone();
        let mut after = None;
        for _ in 0..4 {
            self.loops.push(LoopFrame::default());
            let mut current = head.clone();
            if let (Some(map), Some(bindings)) = (&mut current, bindings) {
                for binding in bindings {
                    map.insert(*binding, Status::Init);
                }
            }
            let exit = body(self, &mut current);
            let frame = self.loops.pop().unwrap_or_default();
            let mut back = current;
            for continued in &frame.continues {
                back = join(&back, continued);
            }
            let mut exits = exit;
            for broken in &frame.breaks {
                exits = join(&exits, broken);
            }
            after = Some(exits);
            let next = join_back_edge(&entry, &back);
            if next == head {
                break;
            }
            head = next;
        }
        *state = after.unwrap_or(None);
    }
}

/// Infers parameter modes and checks ownership for every function.
pub fn check_program(program: &mut Program, components: &[Vec<FuncId>]) -> Vec<Diagnostic> {
    let mut modes: HashMap<FuncId, Vec<ParamMode>> = HashMap::new();
    let names: HashMap<FuncId, String> = program
        .functions
        .iter()
        .enumerate()
        .map(|(i, f)| (FuncId(i as u32), f.name.clone()))
        .collect();
    let constant_names: Vec<String> = program.constants.iter().map(|c| c.name.clone()).collect();
    for (index, function) in program.functions.iter().enumerate() {
        modes.insert(
            FuncId(index as u32),
            function.params.iter().map(|p| p.mode).collect(),
        );
    }
    let mut diagnostics = Vec::new();

    for component in components {
        // Infer modes to a fixed point without reporting.
        for _ in 0..8 {
            let mut changed = false;
            for id in component {
                let inferred = analyze(program, *id, &modes, &names, &constant_names, false).0;
                if modes.get(id) != Some(&inferred) {
                    modes.insert(*id, inferred);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for id in component {
            let function = &mut program.functions[id.0 as usize];
            for (param, mode) in function.params.iter_mut().zip(&modes[id]) {
                param.mode = *mode;
            }
        }
        for id in component {
            let (_, mut found) = analyze(program, *id, &modes, &names, &constant_names, true);
            diagnostics.append(&mut found);
        }
    }

    // Constant initializers.
    for index in 0..program.constants.len() {
        let mut locals = std::mem::take(&mut program.constants[index].locals);
        let mut analyzer = Analyzer {
            modes: &modes,
            names: &names,
            constant_names: &constant_names,
            locals: &mut locals,
            params: HashMap::new(),
            report: true,
            diagnostics: Vec::new(),
            reported: HashSet::new(),
            loops: Vec::new(),
            loop_borrows: Vec::new(),
            regions: Vec::new(),
            reason: MoveReason::Explicit,
        };
        let mut state: State = Some(HashMap::new());
        analyzer.expr(
            &mut program.constants[index].value,
            Ctx::Consume,
            &mut state,
        );
        diagnostics.append(&mut analyzer.diagnostics);
        program.constants[index].locals = locals;
    }
    diagnostics
}

fn analyze(
    program: &mut Program,
    id: FuncId,
    modes: &HashMap<FuncId, Vec<ParamMode>>,
    names: &HashMap<FuncId, String>,
    constant_names: &[String],
    report: bool,
) -> (Vec<ParamMode>, Vec<Diagnostic>) {
    let function = &mut program.functions[id.0 as usize];
    let mut locals = std::mem::take(&mut function.locals);
    let mut body = std::mem::take(&mut function.body);
    let mut params: HashMap<LocalId, (ParamMode, bool)> = HashMap::new();
    let mut state_map = HashMap::new();
    for (param, mode) in function.params.iter().zip(&modes[&id]) {
        let explicit = param.explicit_mode || param.ty.is_copy();
        params.insert(param.local, (*mode, explicit));
        state_map.insert(param.local, Status::Init);
    }
    let mut defaults: Vec<Option<Expr>> = function
        .params
        .iter_mut()
        .map(|p| p.default.take())
        .collect();
    let param_locals: Vec<LocalId> = function.params.iter().map(|p| p.local).collect();

    let mut analyzer = Analyzer {
        modes,
        names,
        constant_names,
        locals: &mut locals,
        params,
        report,
        diagnostics: Vec::new(),
        reported: HashSet::new(),
        loops: Vec::new(),
        loop_borrows: Vec::new(),
        regions: Vec::new(),
        reason: MoveReason::Explicit,
    };
    for default in defaults.iter_mut().flatten() {
        let mut state: State = Some(state_map.clone());
        analyzer.expr(default, Ctx::Consume, &mut state);
    }
    let mut state: State = Some(state_map);
    analyzer.block(&mut body, &mut state);
    let inferred: Vec<ParamMode> = param_locals
        .iter()
        .map(|local| analyzer.params[local].0)
        .collect();
    let diagnostics = std::mem::take(&mut analyzer.diagnostics);

    let function = &mut program.functions[id.0 as usize];
    function.locals = locals;
    function.body = body;
    for (param, default) in function.params.iter_mut().zip(defaults) {
        param.default = default;
    }
    (inferred, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(line: u32) -> MoveSite {
        MoveSite {
            span: Span::new(sayon_syntax::FileId(0), line as usize, line as usize + 1),
            reason: MoveReason::Return,
            earlier_iteration: false,
        }
    }

    #[test]
    fn join_marks_moves_as_maybe() {
        let a: State = Some(HashMap::from([
            (LocalId(0), Status::Init),
            (LocalId(1), Status::Init),
        ]));
        let b: State = Some(HashMap::from([
            (LocalId(0), Status::Moved(site(1))),
            (LocalId(1), Status::Init),
        ]));
        let joined = join(&a, &b).unwrap();
        assert!(matches!(joined[&LocalId(0)], Status::MaybeMoved(_)));
        assert_eq!(joined[&LocalId(1)], Status::Init);
        assert_eq!(join(&None, &b), b);
    }
}
