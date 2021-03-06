use std::{collections::HashMap, ops::Index};

use super::{unify::InferenceTable, Type, *};
use crate::{
    builtins::BuiltinKind,
    diagnostic::Diagnostic,
    hir,
    hir::*,
    scopes::{Denotation, Scopes},
};
use arena::ArenaMap;
use either::{Either, Either::*};

pub fn infer(module: Module, scopes: Scopes) -> InferenceResult {
    let mut ctx = Ctx::new(module, scopes);
    ctx.infer_module();
    ctx.finish()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InferenceResult {
    pub type_of_expr: ArenaMap<ExprId, Type>,
    pub type_of_type: ArenaMap<TypeId, Type>,
    pub type_of_pat: ArenaMap<PatId, Type>,
    pub type_of_fn: ArenaMap<FnDefId, FnType>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Index<ExprId> for InferenceResult {
    type Output = Type;
    fn index(&self, id: ExprId) -> &Self::Output { &self.type_of_expr[id] }
}
impl Index<TypeId> for InferenceResult {
    type Output = Type;
    fn index(&self, id: TypeId) -> &Self::Output { &self.type_of_type[id] }
}
impl Index<PatId> for InferenceResult {
    type Output = Type;
    fn index(&self, id: PatId) -> &Self::Output { &self.type_of_pat[id] }
}
impl Index<FnDefId> for InferenceResult {
    type Output = FnType;
    fn index(&self, id: FnDefId) -> &Self::Output { &self.type_of_fn[id] }
}

/// An id representing any "thing" whose type is infered.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum InferenceId {
    Expr(ExprId),
    Type(TypeId),
    Pat(PatId),
}

struct Ctx {
    module: Module,
    scopes: Scopes,
    result: InferenceResult,
    table: InferenceTable,
    fn_type: Option<FnType>,
    loop_type: Option<Type>,
}

impl Ctx {
    fn new(module: Module, scopes: Scopes) -> Self {
        Self {
            module,
            scopes,
            result: InferenceResult::default(),
            table: InferenceTable::default(),
            fn_type: None,
            loop_type: None,
        }
    }

    fn finish(mut self) -> InferenceResult {
        let mut result = std::mem::take(&mut self.result);
        for (id, ty) in result.type_of_expr.iter_mut() {
            let was_unknown = ty == &Type::Unknown;
            *ty = self.propagate_type_completely(ty);
            if !was_unknown && ty == &Type::Unknown {
                result
                    .diagnostics
                    .push(Diagnostic::InferenceFail(InferenceId::Expr(id)))
            }
        }

        for (id, ty) in result.type_of_type.iter_mut() {
            let was_unknown = ty == &Type::Unknown;
            *ty = self.propagate_type_completely(ty);
            if !was_unknown && ty == &Type::Unknown {
                result
                    .diagnostics
                    .push(Diagnostic::InferenceFail(InferenceId::Type(id)))
            }
        }

        for (id, ty) in result.type_of_pat.iter_mut() {
            let was_unknown = ty == &Type::Unknown;
            *ty = self.propagate_type_completely(ty);
            if !was_unknown && ty == &Type::Unknown {
                result
                    .diagnostics
                    .push(Diagnostic::InferenceFail(InferenceId::Pat(id)))
            }
        }

        for ty in result.type_of_fn.values_mut() {
            *ty = self.propagate_fn_type_completely(ty);
        }

        result
    }

    fn with_fn_type<T>(&mut self, ty: FnType, f: impl Fn(&mut Self) -> T) -> T {
        let old_fn_type = self.fn_type.clone();
        self.fn_type = Some(ty);
        let ret = f(self);
        self.fn_type = old_fn_type;
        ret
    }

    fn with_loop_type<T>(&mut self, ty: Type, f: impl Fn(&mut Self) -> T) -> T {
        let old_loop_type = self.loop_type.clone();
        self.loop_type = Some(ty);
        let ret = f(self);
        self.loop_type = old_loop_type;
        ret
    }

    fn set_expr_type(&mut self, id: ExprId, ty: Type) { self.result.type_of_expr.insert(id, ty); }
    fn set_pat_type(&mut self, id: PatId, ty: Type) { self.result.type_of_pat.insert(id, ty); }
    fn set_type_type(&mut self, id: TypeId, ty: Type) { self.result.type_of_type.insert(id, ty); }
    fn set_fn_type(&mut self, fn_def: FnDefId, ty: FnType) {
        self.result.type_of_fn.insert(fn_def, ty);
    }

    fn new_type_var(&mut self) -> Type { Type::Infer(InferType::Var(self.table.new_type_var())) }

    fn resolve_type(&mut self, id: TypeId) -> Type {
        let ty = self.module.data[id].clone();
        let ty = match ty {
            hir::Type::Infer => self.new_type_var(),
            hir::Type::Tuple(tys) => {
                Type::tuple(tys.iter().map(|ty| self.resolve_type(*ty)).collect())
            }
            hir::Type::Fn { params, ret } => Type::function(
                params.iter().map(|ty| self.resolve_type(*ty)).collect(),
                self.resolve_type(ret),
            ),
            hir::Type::Var(var) => self.resolve_var_type(id, var),
        };
        let ty = self.propagate_type_as_far_as_possible(&ty);
        self.set_type_type(id, ty.clone());
        ty
    }

    fn resolve_var_type(&mut self, id: TypeId, var_id: VarId) -> Type {
        let var = &self.module.data[var_id];
        let denotation = self.scopes.lookup_type(id, var);
        match denotation {
            Some(Denotation::Builtin(b)) if b.kind() == BuiltinKind::Type => b.ty(),
            Some(Denotation::Struct(id)) => Type::struct_(id),
            Some(Denotation::Enum(id)) => Type::enum_(id),
            _ => {
                self.result.diagnostics.push(Diagnostic::UnboundVar {
                    id: Right(id),
                    var: var_id,
                    denotation,
                });
                Type::Unknown
            }
        }
    }

    fn resolve_var_expr(&mut self, id: ExprId, var_id: VarId) -> Type {
        let var = &self.module.data[var_id];
        let denotation = self.scopes.lookup_expr(id, var);
        match denotation {
            Some(Denotation::Local(id)) => self.result.type_of_pat[id].clone(),
            Some(Denotation::Fn(id)) => self.result.type_of_fn[id].clone().into(),
            Some(Denotation::Builtin(b)) if b.kind() == BuiltinKind::Value => b.ty(),
            _ => {
                self.result.diagnostics.push(Diagnostic::UnboundVar {
                    id: Left(id),
                    var: var_id,
                    denotation,
                });
                Type::Unknown
            }
        }
    }

    /// Propagates the type as far as currently possible, replacing type
    /// variables by their known types. All types returned by the `infer_*`
    /// functions should be resolved as far as possible, i.e. contain no
    /// type variables with known type.
    fn propagate_type_as_far_as_possible(&mut self, ty: &Type) -> Type {
        self.table.propagate_type_as_far_as_possible(ty)
    }

    /// Propagates the type completely; type variables without known type are
    /// replaced by `Type::Unknown`.
    fn propagate_type_completely(&mut self, ty: &Type) -> Type {
        self.table
            .propagate_type_completely_inner(&mut Vec::new(), ty)
    }

    fn propagate_fn_type_completely(&mut self, fn_type: &FnType) -> FnType {
        FnType {
            params: fn_type
                .params
                .iter()
                .map(|param| self.propagate_type_completely(param))
                .collect(),
            ret: self.propagate_type_completely(&fn_type.ret),
        }
    }

    fn try_to_unify(&mut self, id: Either<ExprId, PatId>, expected: &Type, got: &Type) -> Type {
        match self.coerce(got, expected) {
            false => {
                self.result.diagnostics.push(Diagnostic::TypeMismatch {
                    id,
                    expected: expected.clone(),
                    got: got.clone(),
                });
                got.clone()
            }
            true if expected == &Type::Unknown => got.clone(),
            true => expected.clone(),
        }
    }

    fn try_to_unify_and_propagate_as_far_as_possible(
        &mut self,
        id: Either<ExprId, PatId>,
        expected: &Type,
        got: &Type,
    ) -> Type {
        let ty = self.try_to_unify(id, expected, got);
        self.propagate_type_as_far_as_possible(&ty)
    }

    fn unify(&mut self, t1: &Type, t2: &Type) -> bool { self.table.unify(t1, t2) }

    fn coerce(&mut self, from: &Type, to: &Type) -> bool {
        if from == &Type::NEVER {
            true
        } else {
            self.unify(from, to)
        }
    }
}

impl Ctx {
    fn infer_module(&mut self) {
        self.module
            .decls
            .clone()
            .iter()
            .copied()
            .for_each(|decl| self.infer_decl(decl));
        self.module
            .decls
            .clone()
            .iter()
            .copied()
            .for_each(|decl| self.infer_decl_body(decl));
    }

    fn infer_decl(&mut self, decl: Decl) {
        match decl {
            Decl::Struct(id) => self.infer_struct_decl(id),
            Decl::Enum(id) => self.infer_enum_decl(id),
            Decl::Fn(id) => self.infer_fn_decl(id),
        }
    }

    fn infer_struct_decl(&mut self, id: StructDefId) {
        let struct_decl = self.module.data[id].clone();
        for field in struct_decl.fields {
            self.resolve_type(field.ty);
        }
    }

    fn infer_enum_decl(&mut self, id: EnumDefId) {
        let enum_decl = self.module.data[id].clone();
        for variant in enum_decl.variants {
            for field in variant.fields {
                self.resolve_type(field.ty);
            }
        }
    }

    fn infer_fn_decl(&mut self, fn_id: FnDefId) {
        let fn_decl = self.module.data[fn_id].clone();
        let params = fn_decl
            .params
            .iter()
            .map(|param| self.infer_binding(param.pat, param.ty, None))
            .collect();
        let ret = fn_decl
            .ret_type
            .map_or(Type::UNIT, |ty| self.resolve_type(ty));
        self.set_fn_type(fn_id, FnType { params, ret });
    }

    fn infer_decl_body(&mut self, decl: Decl) {
        match decl {
            Decl::Struct(_) | Decl::Enum(_) => {}
            Decl::Fn(fn_id) => {
                self.infer_fn_body(fn_id);
            }
        };
    }

    fn infer_fn_body(&mut self, fn_id: FnDefId) -> Type {
        let fn_decl = self.module.data[fn_id].clone();
        let fn_type = self.result.type_of_fn[fn_id].clone();
        let body_type = self.with_fn_type(fn_type.clone(), |this| {
            this.infer_expr(&Type::Unknown, fn_decl.expr)
        });
        self.try_to_unify_and_propagate_as_far_as_possible(
            Left(fn_decl.expr),
            &fn_type.ret,
            &body_type,
        )
    }

    /// Common inference logic for any construct that binds a value to a pattern
    /// (ie fn params, lambda params, let bindings)
    /// type annotation takes priority over the expr, which takes priority over
    /// the pattern
    /// eg `let (x,y): (Bool,Bool,Bool) = (1,2,3);` will create an error
    /// pointing at the expr and the pattern, not at the annotation. This is
    /// because, when the user provides a type annotation, they usually want
    /// it to guide the inference process on expr inference (eg `let x:
    /// Vec<_> = xs.iter().collect()`)
    fn infer_binding(&mut self, pat: PatId, ty: Option<TypeId>, expr: Option<ExprId>) -> Type {
        let ty = match ty {
            Some(ty) => self.resolve_type(ty),
            None => self.new_type_var(),
        };
        let expr_ty = match expr {
            Some(expr) => self.infer_expr(&ty, expr),
            None => ty,
        };
        let expr_ty = self.propagate_type_as_far_as_possible(&expr_ty);
        self.infer_pat(&expr_ty, pat)
    }

    fn infer_pat(&mut self, expected: &Type, id: PatId) -> Type {
        let pat = self.module.data[id].clone();
        let ty = match pat {
            Pat::Var(_) | Pat::Ignore => expected.clone(),
            Pat::Tuple(pats) => {
                let expectations = expected.as_tuple().unwrap_or(&[]);
                let expectations = expectations.iter().chain(std::iter::repeat(&Type::Unknown));
                let tys = pats
                    .iter()
                    .zip(expectations)
                    .map(|(pat, expected)| self.infer_pat(expected, *pat))
                    .collect();
                Type::tuple(tys)
            }
        };
        let ty = self.propagate_type_as_far_as_possible(&ty);
        self.set_pat_type(id, ty.clone());
        self.try_to_unify_and_propagate_as_far_as_possible(Right(id), expected, &ty)
    }

    fn infer_expr(&mut self, expected: &Type, id: ExprId) -> Type {
        let expr = self.module.data[id].clone();
        let ty = match expr {
            Expr::Lit(lit) => lit.ty(),
            Expr::Var(var) => self.resolve_var_expr(id, var),
            Expr::Tuple(exprs) => self.infer_tuple_expr(expected, &exprs),
            Expr::Struct { name, fields } => self.infer_struct_expr(id, name, &fields),
            Expr::Enum {
                name,
                variant,
                fields,
            } => self.infer_enum_expr(id, name, variant, &fields),
            Expr::If {
                test,
                then_branch,
                else_branch,
            } => self.infer_if_expr(test, then_branch, else_branch),
            Expr::Lambda { params, expr } => self.infer_lambda_expr(expected, &params, expr),
            Expr::Call { func, args } => self.infer_call_expr(func, &args),
            Expr::Field { expr, field } => self.infer_field_expr(expr, field),
            Expr::Unop { op, expr } => self.infer_unop_expr(op, expr),
            Expr::Binop { lhs, op, rhs } => self.infer_binop_expr(op, lhs, rhs),
            Expr::Loop(expr) => self.infer_loop_expr(expected, expr),
            Expr::Return(expr) => self.infer_return_expr(id, expr),
            Expr::Break(expr) => self.infer_break_expr(id, expr),
            Expr::Continue => self.infer_continue_expr(id),
            Expr::Block { stmts, expr } => self.infer_block_expr(expected, &stmts, expr),
        };
        let ty = self.propagate_type_as_far_as_possible(&ty);
        self.set_expr_type(id, ty.clone());
        self.try_to_unify_and_propagate_as_far_as_possible(Left(id), expected, &ty)
    }

    fn infer_tuple_expr(&mut self, expected: &Type, exprs: &[ExprId]) -> Type {
        let expectations = expected.as_tuple().unwrap_or(&[]);
        let expectations = expectations.iter().chain(std::iter::repeat(&Type::Unknown));
        let tys = exprs
            .iter()
            .zip(expectations)
            .map(|(expr, expected)| self.infer_expr(expected, *expr))
            .collect();
        Type::tuple(tys)
    }

    fn infer_struct_expr(&mut self, expr: ExprId, name: VarId, fields: &[FieldInit]) -> Type {
        let var = &self.module.data[name];
        let denotation = self.scopes.lookup_expr(expr, var);
        match denotation {
            Some(Denotation::Struct(id)) => {
                let struct_def = self.module.data[id].clone();
                let struct_type = Type::struct_(id);
                self.infer_fields(expr, Some(&struct_def.fields), fields);
                struct_type
            }
            _ => {
                self.infer_fields(expr, None, fields);
                Type::Unknown
            }
        }
    }

    fn infer_enum_expr(
        &mut self,
        expr: ExprId,
        name: VarId,
        variant: VarId,
        fields: &[FieldInit],
    ) -> Type {
        let var = &self.module.data[name];
        let denotation = self.scopes.lookup_expr(expr, var);
        match denotation {
            Some(Denotation::Enum(id)) => {
                let enum_def = self.module.data[id].clone();
                let enum_type = Type::enum_(id);
                let variant = enum_def
                    .variants
                    .iter()
                    .find(|v| self.module.data[v.name] == self.module.data[variant]);
                match variant {
                    None => todo!("No such variant"),
                    Some(variant) => {
                        self.infer_fields(expr, Some(&variant.fields), fields);
                    }
                }
                enum_type
            }
            _ => {
                self.infer_fields(expr, None, fields);
                Type::Unknown
            }
        }
    }

    fn infer_fields(&mut self, expr: ExprId, fields: Option<&[StructField]>, inits: &[FieldInit]) {
        let mut first_init = HashMap::new();

        for init in inits {
            let name = self.module.data[init.name].clone();
            match first_init.get(&name) {
                None => {
                    first_init.insert(name, init.val);
                }
                Some(_) => todo!("Duplicate field init"),
            }

            let expected = fields
                .and_then(|fields| {
                    let field = fields
                        .iter()
                        .find(|field| self.module.data[field.name] == self.module.data[init.name]);
                    match field {
                        None => {
                            self.result.diagnostics.push(Diagnostic::NoSuchField {
                                field: Field::Named(init.name),
                                expr: init.val,
                                possible_fields: Left(fields.to_vec()),
                            });
                            None
                        }
                        Some(field) => Some(self.result.type_of_type[field.ty].clone()),
                    }
                })
                .unwrap_or(Type::Unknown);
            self.infer_expr(&expected, init.val);
        }

        if let Some(fields) = fields {
            for field in fields {
                let name = &self.module.data[field.name];
                let init = inits.iter().find(|f| &self.module.data[f.name] == name);
                match init {
                    Some(_) => {}
                    None => {
                        self.result.diagnostics.push(Diagnostic::MissingField {
                            expr,
                            field: Field::Named(field.name),
                        });
                    }
                }
            }
        }
    }

    fn infer_if_expr(
        &mut self,
        test: ExprId,
        then_branch: ExprId,
        else_branch: Option<ExprId>,
    ) -> Type {
        let _test_ty = self.infer_expr(&Type::BOOL, test);
        match else_branch {
            None => {
                self.infer_expr(&Type::Unknown, then_branch);
                Type::UNIT
            }
            Some(else_branch) => {
                let then_ty = self.infer_expr(&Type::Unknown, then_branch);
                let else_ty = self.infer_expr(&Type::Unknown, else_branch);
                if self.unify(&then_ty, &else_ty) {
                    then_ty
                } else {
                    self.result.diagnostics.push(Diagnostic::IfBranchMismatch {
                        then_branch,
                        else_branch,
                        then_ty,
                        else_ty,
                    });
                    Type::Unknown
                }
            }
        }
    }

    fn infer_lambda_expr(&mut self, expected: &Type, params: &[Param], body: ExprId) -> Type {
        let param_types = params
            .iter()
            .map(|param| self.infer_binding(param.pat, param.ty, None))
            .collect::<Vec<_>>();
        let ret_type = self.new_type_var();
        let lambda_ty = FnType {
            params: param_types,
            ret: ret_type.clone(),
        };
        self.unify(&lambda_ty.clone().into(), expected);
        self.with_fn_type(lambda_ty.clone(), |this| this.infer_expr(&ret_type, body));
        lambda_ty.into()
    }

    fn infer_call_expr(&mut self, func: ExprId, args: &[ExprId]) -> Type {
        let func_ty = self.infer_expr(&Type::Unknown, func);
        match func_ty.as_fn() {
            None => {
                self.result.diagnostics.push(Diagnostic::CalledNonFn {
                    expr: func,
                    ty: func_ty,
                });
                Type::Unknown
            }
            Some(FnType { params, ret }) => {
                if args.len() != params.len() {
                    self.result.diagnostics.push(Diagnostic::ArgCountMismatch {
                        expr: func,
                        ty: func_ty,
                        expected: params.len(),
                        got: args.len(),
                    });
                }
                for (arg, param) in args.iter().zip(params.iter()) {
                    self.infer_expr(param, *arg);
                }
                ret
            }
        }
    }

    fn infer_field_expr(&mut self, base: ExprId, field: Field) -> Type {
        let base_type = self.infer_expr(&Type::Unknown, base);
        match base_type {
            Type::App {
                ctor: Ctor::Tuple,
                ref params,
            } => match field {
                Field::Tuple(idx) if (idx as usize) < params.len() => params[idx as usize].clone(),
                Field::Tuple(_) | Field::Named(_) => {
                    self.result.diagnostics.push(Diagnostic::NoSuchField {
                        expr: base,
                        possible_fields: Right(params.len() as u32),
                        field,
                    });
                    Type::Unknown
                }
            },
            Type::App {
                ctor: Ctor::Struct(id),
                ..
            } => {
                let struct_def = &self.module.data[id];
                match field {
                    Field::Named(name) => {
                        let field_name = &self.module.data[name];
                        let target = struct_def
                            .fields
                            .iter()
                            .find(|field| &self.module.data[field.name] == field_name);
                        match target {
                            Some(field) => self.result.type_of_type[field.ty].clone(),
                            None => {
                                self.result.diagnostics.push(Diagnostic::NoSuchField {
                                    expr: base,
                                    possible_fields: Left(struct_def.fields.clone()),
                                    field,
                                });
                                Type::Unknown
                            }
                        }
                    }
                    Field::Tuple(_) => {
                        self.result.diagnostics.push(Diagnostic::NoSuchField {
                            expr: base,
                            possible_fields: Left(struct_def.fields.clone()),
                            field,
                        });
                        Type::Unknown
                    }
                }
            }
            _ => {
                self.result.diagnostics.push(Diagnostic::NoFields {
                    expr: base,
                    ty: base_type,
                });
                Type::Unknown
            }
        }
    }

    fn infer_unop_expr(&mut self, op: Unop, lhs: ExprId) -> Type {
        let lhs_expectation = op.lhs_expectation();
        let lhs_type = self.infer_expr(&lhs_expectation, lhs);
        op.return_type(&lhs_type)
    }

    fn is_lvalue(&self, id: ExprId) -> bool {
        let expr = &self.module.data[id];
        matches!(expr, Expr::Var(_) | Expr::Field { .. })
    }

    fn infer_binop_expr(&mut self, op: Binop, lhs: ExprId, rhs: ExprId) -> Type {
        if let Binop::Assign = op {
            if !self.is_lvalue(lhs) {
                self.result.diagnostics.push(Diagnostic::NotLValue { lhs });
            }
        }

        let lhs_expectation = op.lhs_expectation();
        let lhs_type = self.infer_expr(&lhs_expectation, lhs);
        let rhs_expectation = op.rhs_expectation(&lhs_type);
        if lhs_type != Type::Unknown && rhs_expectation == Type::Unknown {
            self.result.diagnostics.push(Diagnostic::CannotApplyBinop {
                lhs_type,
                rhs_type: rhs_expectation.clone(),
                op,
            });
        }
        let rhs_type = self.infer_expr(&rhs_expectation, rhs);
        op.return_type(&rhs_type)
    }

    fn infer_loop_expr(&mut self, expected: &Type, expr: ExprId) -> Type {
        self.with_loop_type(Type::NEVER, |this| {
            this.infer_expr(expected, expr);
            this.loop_type.clone().unwrap()
        })
    }

    fn infer_return_expr(&mut self, parent_expr: ExprId, expr: Option<ExprId>) -> Type {
        let result_type = expr.map_or(Type::UNIT, |expr| self.infer_expr(&Type::Unknown, expr));
        match self.fn_type.clone() {
            None => {
                self.result
                    .diagnostics
                    .push(Diagnostic::ReturnNotInFn(expr.unwrap_or(parent_expr)));
            }
            Some(fn_type) => {
                self.try_to_unify_and_propagate_as_far_as_possible(
                    Left(expr.unwrap_or(parent_expr)),
                    &fn_type.ret,
                    &result_type,
                );
            }
        };
        Type::NEVER
    }

    fn infer_break_expr(&mut self, parent_expr: ExprId, expr: Option<ExprId>) -> Type {
        let result_type = expr.map_or(Type::UNIT, |expr| self.infer_expr(&Type::Unknown, expr));
        match self.loop_type.as_mut() {
            None => {
                self.result
                    .diagnostics
                    .push(Diagnostic::BreakNotInLoop(expr.unwrap_or(parent_expr)));
            }
            Some(loop_type) => *loop_type = result_type,
        }
        Type::NEVER
    }

    fn infer_continue_expr(&mut self, parent_expr: ExprId) -> Type {
        match self.loop_type.clone() {
            None => {
                self.result
                    .diagnostics
                    .push(Diagnostic::BreakNotInLoop(parent_expr));
            }
            Some(_) => {}
        }
        Type::NEVER
    }

    fn infer_block_expr(&mut self, expected: &Type, stmts: &[Stmt], expr: Option<ExprId>) -> Type {
        for stmt in stmts {
            match stmt {
                Stmt::Expr(expr) => self.infer_expr(&Type::Unknown, *expr),
                Stmt::Let { pat, ty, expr } => self.infer_binding(*pat, *ty, Some(*expr)),
            };
        }

        match expr {
            Some(expr) => self.infer_expr(expected, expr),
            None => Type::UNIT,
        }
    }
}

impl Lit {
    const fn ty(self) -> Type {
        match self {
            Self::Bool(_) => Type::BOOL,
            Self::Int(_) => Type::INT,
            Self::Float(_) => Type::FLOAT,
            Self::Char(_) => Type::CHAR,
        }
    }
}

impl Unop {
    const fn lhs_expectation(self) -> Type {
        match self {
            Self::Add | Self::Sub => Type::Unknown,
            Self::Not => Type::BOOL,
        }
    }

    fn return_type(self, lhs_type: &Type) -> Type {
        match self {
            Self::Add | Self::Sub => lhs_type.clone(),
            Self::Not => Type::BOOL,
        }
    }
}
impl Binop {
    const fn lhs_expectation(self) -> Type {
        match self {
            Self::Lazy(LazyBinop::And | LazyBinop::Or) => Type::BOOL,
            Self::Arithmetic(_) | Self::Cmp(_) | Self::Assign => Type::Unknown,
        }
    }

    fn rhs_expectation(self, lhs_type: &Type) -> Type {
        match self {
            Self::Lazy(LazyBinop::And | LazyBinop::Or) => Type::BOOL,
            Self::Arithmetic(_) | Self::Cmp(_) | Self::Assign => lhs_type.clone(),
        }
    }

    fn return_type(self, rhs_type: &Type) -> Type {
        match self {
            Self::Lazy(LazyBinop::And | LazyBinop::Or) => Type::BOOL,
            Self::Arithmetic(_) => rhs_type.clone(),
            Self::Cmp(_) => Type::BOOL,
            Self::Assign => Type::UNIT,
        }
    }
}
