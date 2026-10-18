//! Checks items component by component and assembles the program.

use crate::checker::{Checker, ConstState, FnCtx, FunctionEntry, Item, Signature};
use crate::hir::{self, ConstId, FuncId, LocalKind};
use crate::narrow::block_exits;
use crate::types::Type;
use sayon_syntax::Diagnostic;
use sayon_syntax::ast;

impl<'a> Checker<'a> {
    pub fn check_component(&mut self, component: &[Item], init_order: &mut Vec<ConstId>) {
        let constants: Vec<ConstId> = component
            .iter()
            .filter_map(|item| {
                if let Item::Constant(id) = item {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        let functions: Vec<FuncId> = component
            .iter()
            .filter_map(|item| {
                if let Item::Function(id) = item {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();

        if !constants.is_empty() && component.len() > 1 {
            let first = &self.constants[constants[0].0 as usize];
            let mut diagnostic = Diagnostic::error(
                "E_INITIALIZATION_CYCLE",
                format!("the constant `{}` depends on itself", first.decl.name.name),
            )
            .primary(first.span, "");
            for item in &component[1..] {
                let (span, name) = match item {
                    Item::Function(id) => (
                        self.functions[id.0 as usize].span,
                        &self.functions[id.0 as usize].name,
                    ),
                    Item::Constant(id) => (
                        self.constants[id.0 as usize].span,
                        &self.constants[id.0 as usize].decl.name.name,
                    ),
                    Item::Type(id) => (
                        self.types[id.0 as usize].span,
                        &self.types[id.0 as usize].name,
                    ),
                };
                diagnostic = diagnostic.secondary(span, format!("`{name}` is part of the cycle"));
            }
            self.error(diagnostic.help("constants are initialized before the program starts and cannot depend on themselves"));
            for id in constants {
                self.constants[id.0 as usize].ty = Some(Type::Unknown);
                self.constants[id.0 as usize].state = ConstState::Done;
            }
            for id in &functions {
                let signature = self.initial_signature(*id);
                self.functions[id.0 as usize].signature = Some(signature);
                self.functions[id.0 as usize].generalized = true;
            }
            return;
        }
        if let [id] = constants.as_slice() {
            if self.uses_itself(*id) {
                let constant = &self.constants[id.0 as usize];
                let diagnostic = Diagnostic::error(
                    "E_INITIALIZATION_CYCLE",
                    format!(
                        "the constant `{}` is used in its own initializer",
                        constant.decl.name.name
                    ),
                )
                .primary(constant.span, "");
                self.error(diagnostic);
                self.constants[id.0 as usize].ty = Some(Type::Unknown);
                return;
            }
            self.check_constant(*id);
            init_order.push(*id);
            return;
        }
        self.check_functions(&functions);
    }

    fn uses_itself(&self, id: ConstId) -> bool {
        let constant = &self.constants[id.0 as usize];
        let mut names = Vec::new();
        crate::checker::collect_expr_names(&constant.decl.value, &mut names);
        names.contains(&constant.decl.name.name)
    }

    fn check_constant(&mut self, id: ConstId) {
        let first_var = self.unifier.var_count();
        let first_closure = self.closures.len();
        let decl = self.constants[id.0 as usize].decl;
        self.constants[id.0 as usize].state = ConstState::InProgress;
        let module = self.constants[id.0 as usize].module;
        let mut cx = FnCtx::new(module, None, Type::Nothing, false);
        let annotation = decl
            .ty
            .as_ref()
            .map(|ty| self.resolve_type(module, ty, &Default::default()));
        let mut value = match &annotation {
            Some(ty) => self.check_expr_as(&mut cx, &decl.value, ty, "constant value"),
            None => self.check_expr(&mut cx, &decl.value, None),
        };
        self.solve_deferred(true);
        self.default_vars(first_var);
        self.finalize_expr(&mut value);
        self.finish_closures(first_closure);
        let ty = annotation
            .map(|t| self.finalize_type(&t))
            .unwrap_or_else(|| value.ty.clone());
        if matches!(ty, Type::View(..)) {
            self.view_escape(decl.name.span, "a constant cannot hold a view".to_string());
        }
        if holds_wrapper(&self.type_defs, &ty) {
            self.error(
                Diagnostic::error(
                    "E_SHARED_CONSTANT",
                    format!("a constant cannot hold {ty}"),
                )
                .primary(decl.name.span, "")
                .note("a constant never changes, and is the same for the whole program; Shared, Weak, and Cell values are for state that is owned and changed")
                .help("create the value in `main` or a function, and pass it where it is needed"),
            );
        }
        let entry = &mut self.constants[id.0 as usize];
        entry.ty = Some(ty);
        entry.hir = Some(value);
        entry.state = ConstState::Done;
    }

    fn check_functions(&mut self, functions: &[FuncId]) {
        let first_var = self.unifier.var_count();
        let first_closure = self.closures.len();
        for id in functions {
            let signature = self.initial_signature(*id);
            self.functions[id.0 as usize].signature = Some(signature);
        }
        if functions.len() > 1 {
            for id in functions {
                let entry = &self.functions[id.0 as usize];
                if entry
                    .signature
                    .as_ref()
                    .is_some_and(|s| s.explicit_generics > 0)
                {
                    let span = entry.span;
                    self.unavailable(span, "mutually recursive generic functions", "M3");
                }
            }
        }
        self.current_component = functions.to_vec();
        self.unifier.param_traits.clear();
        let mut bodies = Vec::new();
        for id in functions {
            bodies.push(self.check_function_body(*id));
        }
        self.current_component.clear();
        self.solve_deferred(true);
        // A body that clones values of an explicit type parameter, or
        // converts them to or from JSON, requires callers to give a type
        // that supports it. (A component with explicit type parameters has
        // one function.)
        let required = std::mem::take(&mut self.unifier.param_traits);
        if let [id] = functions
            && let Some(signature) = &mut self.functions[id.0 as usize].signature
        {
            for (index, traits) in required {
                if index < signature.explicit_generics
                    && let Some(bounds) = signature.bounds.get_mut(index as usize)
                {
                    bounds.traits = bounds.traits.union(traits);
                }
            }
        }

        if functions.len() == 1 {
            self.generalize(functions[0]);
        } else {
            for id in functions {
                self.functions[id.0 as usize].generalized = true;
            }
        }
        self.default_vars(first_var);

        for (id, mut function) in functions.iter().zip(bodies) {
            self.finalize_function(&mut function);
            let signature = self.functions[id.0 as usize]
                .signature
                .clone()
                .expect("signature");
            function.generic_count = signature.generic_count;
            function.numeric_generics = numeric_generics(&signature);
            if signature.generic_count > 0 {
                let identity: Vec<Type> = (0..signature.generic_count).map(Type::Param).collect();
                fill_recursive_type_args(&mut function, *id, &identity);
            }
            let returns_value = !matches!(function.ret, Type::Nothing | Type::Unknown);
            if returns_value && !block_exits(&function.body) {
                let span = self.functions[id.0 as usize]
                    .ast
                    .map(|f| f.header_span)
                    .unwrap_or(function.span);
                self.error(
                    Diagnostic::error(
                        "E_MISSING_RETURN",
                        format!(
                            "`{}` returns {} but can reach its end without returning",
                            function.name, function.ret
                        ),
                    )
                    .primary(span, "")
                    .help("add a `return` on every path, for example at the end of the function"),
                );
            }
            // Store the final signature types (with generic parameters).
            let entry = &mut self.functions[id.0 as usize];
            if let Some(signature) = &mut entry.signature {
                signature.params = function.params.iter().map(|p| p.ty.clone()).collect();
                signature.ret = function.ret.clone();
            }
            entry.hir = Some(function);
        }
        self.finish_closures(first_closure);
        self.scc_order.push(functions.to_vec());
    }

    /// Finalizes the function literals checked since `first`. A literal
    /// shares the type parameters of the named function it appears in.
    fn finish_closures(&mut self, first: usize) {
        for index in first..self.closures.len() {
            let (closure, owner) = self.closures[index];
            let Some(mut function) = self.functions[closure.0 as usize].hir.take() else {
                continue;
            };
            self.finalize_function(&mut function);
            let signature =
                owner.and_then(|owner| self.functions[owner.0 as usize].signature.as_ref());
            let generic_count = signature.map_or(0, |signature| signature.generic_count);
            function.generic_count = generic_count;
            function.numeric_generics = signature.map(numeric_generics).unwrap_or_default();
            if let Some(owner) = owner
                && generic_count > 0
            {
                let identity: Vec<Type> = (0..generic_count).map(Type::Param).collect();
                fill_recursive_type_args(&mut function, owner, &identity);
            }
            self.functions[closure.0 as usize].hir = Some(function);
        }
    }

    fn check_function_body(&mut self, id: FuncId) -> hir::Function {
        let ast = self.functions[id.0 as usize].ast.expect("function source");
        let signature = self.functions[id.0 as usize]
            .signature
            .clone()
            .expect("signature");
        let module = self.functions[id.0 as usize].module;
        let mut cx = FnCtx::new(module, Some(id), signature.ret.clone(), false);
        cx.generics = ast
            .generics
            .iter()
            .enumerate()
            .map(|(i, g)| (g.name.clone(), i as u32))
            .collect();

        let mut params = Vec::new();
        for (index, param) in ast.params.iter().enumerate() {
            let ty = signature.params[index].clone();
            let default = param
                .default
                .as_ref()
                .map(|default| self.check_expr_as(&mut cx, default, &ty, "default value"));
            if let Some(existing) = cx.lookup(&param.name.name) {
                let previous = cx.locals[existing.0 as usize].span;
                self.error(
                    Diagnostic::error(
                        "E_DUPLICATE_DECLARATION",
                        format!("the parameter `{}` appears twice", param.name.name),
                    )
                    .primary(param.name.span, "")
                    .secondary(previous, "first parameter with this name"),
                );
            }
            let local = cx.declare(
                &param.name.name,
                ty.clone(),
                param.name.span,
                LocalKind::Param,
            );
            params.push(hir::Param {
                local,
                mode: param
                    .mode
                    .map(|(mode, _)| mode)
                    .unwrap_or(ast::ParamMode::Read),
                // Calling a function value borrows it for changes, so how a
                // parameter holding one is passed is inferred even when its
                // type is written.
                explicit_mode: param.mode.is_some()
                    || (param.ty.is_some() && !self.holds_function(&self.unifier.resolve(&ty))),
                ty,
                default,
                span: param.span,
            });
        }
        let body = self.check_block(&mut cx, &ast.body);
        if ast.ret.is_none() && !block_exits(&body) {
            // Reaching the end returns nothing.
            if self.unifier.unify(&signature.ret, &Type::Nothing).is_err() {
                let ret = self.unifier.resolve(&signature.ret);
                self.error(
                    Diagnostic::error(
                        "E_MISSING_RETURN",
                        format!(
                            "`{}` returns {ret} on some paths but can also reach its end",
                            self.functions[id.0 as usize].name
                        ),
                    )
                    .primary(ast.header_span, "")
                    .help(
                        "return a value on every path, or annotate the result as `-> T or nothing`",
                    ),
                );
            }
        }
        hir::Function {
            name: self.functions[id.0 as usize].name.clone(),
            span: self.functions[id.0 as usize].span,
            generic_count: 0,
            params,
            ret: signature.ret,
            locals: cx.locals,
            body,
            is_entry: false,
            closure: false,
            captures: Vec::new(),
            numeric_generics: Vec::new(),
        }
    }

    /// Determines the program's entry: the script's top-level statements, or
    /// `main` when the file has no top-level statements.
    pub fn check_entry(&mut self, script: &[&'a ast::Stmt], span: sayon_syntax::Span) -> FuncId {
        let has_statements = script.iter().any(|stmt| {
            !matches!(stmt.kind, ast::StmtKind::Declare(ref decl) if decl.kind == ast::DeclKind::Constant)
        });
        if !has_statements && let Some(Item::Function(main)) = self.item(self.entry_module, "main")
        {
            self.validate_main(main);
            if let Some(function) = &mut self.functions[main.0 as usize].hir {
                function.is_entry = true;
            }
            return main;
        }
        let first_var = self.unifier.var_count();
        let first_closure = self.closures.len();
        let mut cx = FnCtx::new(self.entry_module, None, Type::Nothing, true);
        let stmts = self.check_stmts(&mut cx, script.iter().copied());
        self.solve_deferred(true);
        self.default_vars(first_var);
        let scope = cx.scopes.pop().expect("scope");
        let mut function = hir::Function {
            name: "<script>".to_string(),
            span,
            generic_count: 0,
            params: Vec::new(),
            ret: Type::Nothing,
            locals: cx.locals,
            body: hir::Block {
                stmts,
                locals: scope.declared,
            },
            is_entry: true,
            closure: false,
            captures: Vec::new(),
            numeric_generics: Vec::new(),
        };
        self.finalize_function(&mut function);
        self.finish_closures(first_closure);
        let id = FuncId(self.functions.len() as u32);
        self.functions.push(FunctionEntry {
            ast: None,
            module: self.entry_module,
            name: function.name.clone(),
            span,
            signature: None,
            generalized: true,
            hir: Some(function),
        });
        self.scc_order.push(vec![id]);
        id
    }

    /// Checks each test block as a function without parameters.
    pub fn check_tests(&mut self) -> Vec<(String, FuncId)> {
        let mut tests = Vec::new();
        for (module, name, span, body) in std::mem::take(&mut self.test_blocks) {
            let first_var = self.unifier.var_count();
            let first_closure = self.closures.len();
            let mut cx = FnCtx::new(module, None, Type::Nothing, false);
            let block = self.check_block(&mut cx, body);
            self.solve_deferred(true);
            self.default_vars(first_var);
            let mut function = hir::Function {
                name: format!("test \"{name}\""),
                span,
                generic_count: 0,
                params: Vec::new(),
                ret: Type::Nothing,
                locals: cx.locals,
                body: block,
                is_entry: false,
                closure: false,
                captures: Vec::new(),
                numeric_generics: Vec::new(),
            };
            self.finalize_function(&mut function);
            self.finish_closures(first_closure);
            let id = FuncId(self.functions.len() as u32);
            self.functions.push(FunctionEntry {
                ast: None,
                module,
                name: function.name.clone(),
                span,
                signature: None,
                generalized: true,
                hir: Some(function),
            });
            self.scc_order.push(vec![id]);
            tests.push((name, id));
        }
        tests
    }

    fn validate_main(&mut self, main: FuncId) {
        let entry = &self.functions[main.0 as usize];
        let Some(function) = &entry.hir else { return };
        let span = entry.ast.map(|f| f.header_span).unwrap_or(entry.span);
        let has_params = !function.params.is_empty();
        let ret = function.ret.clone();
        if has_params {
            self.error(
                Diagnostic::error("E_MAIN_SIGNATURE", "`main` cannot take parameters")
                    .primary(span, "")
                    .help("read command-line arguments through the os module instead"),
            );
        }
        if !matches!(ret, Type::Nothing | Type::Unknown) && ret != Type::INTEGER {
            self.error(
                Diagnostic::error(
                    "E_MAIN_SIGNATURE",
                    format!("`main` returns {ret}, but it may only return nothing or integer"),
                )
                .primary(span, ""),
            );
        }
    }
}

/// Whether a value of the type can hold a `Shared`, `Weak`, or `Cell`.
fn holds_wrapper(types: &[crate::types::TypeDef], ty: &Type) -> bool {
    fn search(types: &[crate::types::TypeDef], ty: &Type, seen: &mut Vec<u32>) -> bool {
        match ty {
            Type::Wrapper(..) => true,
            Type::Union(members) => members.iter().any(|m| search(types, m, seen)),
            Type::Array(element) | Type::Dict(_, element) | Type::View(element, _) => {
                search(types, element, seen)
            }
            Type::Named(named) => {
                if seen.contains(&named.id.0) {
                    return false;
                }
                seen.push(named.id.0);
                let def = &types[named.id.0 as usize];
                let variant_fields = def.variants().iter().flat_map(|v| &v.fields);
                def.fields()
                    .iter()
                    .chain(variant_fields)
                    .any(|field| search(types, &field.ty.substitute(&named.args), seen))
            }
            _ => false,
        }
    }
    search(types, ty, &mut Vec::new())
}

fn numeric_generics(signature: &Signature) -> Vec<bool> {
    signature
        .bounds
        .iter()
        .map(|bounds| bounds.only_numbers())
        .collect()
}

fn fill_recursive_type_args(function: &mut hir::Function, id: FuncId, identity: &[Type]) {
    let mut fill = |expr: &mut hir::Expr| {
        if let hir::ExprKind::Call {
            function: callee,
            type_args,
            ..
        }
        | hir::ExprKind::FunctionRef {
            function: callee,
            type_args,
        } = &mut expr.kind
            && *callee == id
            && type_args.is_empty()
        {
            *type_args = identity.to_vec();
        }
    };
    for param in &mut function.params {
        if let Some(default) = &mut param.default {
            hir::visit_expr_mut(default, &mut fill);
        }
    }
    hir::visit_block_mut(&mut function.body, &mut fill);
}
