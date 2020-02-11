use super::{ImplTraitContext, LoweringContext, ParamMode, ParenthesizedGenericArgs};

use rustc::bug;
use rustc_ast::ast::*;
use rustc_ast::attr;
use rustc_ast::ptr::P as AstP;
use rustc_data_structures::thin_vec::ThinVec;
use rustc_errors::struct_span_err;
use rustc_hir as hir;
use rustc_hir::def::Res;
use rustc_span::source_map::{respan, DesugaringKind, Span, Spanned};
use rustc_span::symbol::{sym, Symbol};
use rustc_span::DUMMY_SP;

impl<'hir> LoweringContext<'_, 'hir> {
    fn lower_exprs(&mut self, exprs: &[AstP<Expr>]) -> &'hir [hir::Expr<'hir>] {
        self.arena.alloc_from_iter(exprs.iter().map(|x| self.lower_expr_mut(x)))
    }

    pub(super) fn lower_expr(&mut self, e: &Expr) -> &'hir hir::Expr<'hir> {
        self.arena.alloc(self.lower_expr_mut(e))
    }

    pub(super) fn lower_expr_mut(&mut self, e: &Expr) -> hir::Expr<'hir> {
        let kind = match e.kind {
            ExprKind::Box(ref inner) => hir::ExprKind::Box(self.lower_expr(inner)),
            ExprKind::Array(ref exprs) => hir::ExprKind::Array(self.lower_exprs(exprs)),
            ExprKind::Repeat(ref expr, ref count) => {
                let expr = self.lower_expr(expr);
                let count = self.lower_anon_const(count);
                hir::ExprKind::Repeat(expr, count)
            }
            ExprKind::Tup(ref elts) => hir::ExprKind::Tup(self.lower_exprs(elts)),
            ExprKind::Call(ref f, ref args) => {
                let f = self.lower_expr(f);
                hir::ExprKind::Call(f, self.lower_exprs(args))
            }
            ExprKind::MethodCall(ref seg, ref args) => {
                let hir_seg = self.arena.alloc(self.lower_path_segment(
                    e.span,
                    seg,
                    ParamMode::Optional,
                    0,
                    ParenthesizedGenericArgs::Err,
                    ImplTraitContext::disallowed(),
                    None,
                ));
                let args = self.lower_exprs(args);
                hir::ExprKind::MethodCall(hir_seg, seg.ident.span, args)
            }
            ExprKind::Binary(binop, ref lhs, ref rhs) => {
                let binop = self.lower_binop(binop);
                let lhs = self.lower_expr(lhs);
                let rhs = self.lower_expr(rhs);
                hir::ExprKind::Binary(binop, lhs, rhs)
            }
            ExprKind::Unary(op, ref ohs) => {
                let op = self.lower_unop(op);
                let ohs = self.lower_expr(ohs);
                hir::ExprKind::Unary(op, ohs)
            }
            ExprKind::Lit(ref l) => hir::ExprKind::Lit(respan(l.span, l.kind.clone())),
            ExprKind::Cast(ref expr, ref ty) => {
                let expr = self.lower_expr(expr);
                let ty = self.lower_ty(ty, ImplTraitContext::disallowed());
                hir::ExprKind::Cast(expr, ty)
            }
            ExprKind::Type(ref expr, ref ty) => {
                let expr = self.lower_expr(expr);
                let ty = self.lower_ty(ty, ImplTraitContext::disallowed());
                hir::ExprKind::Type(expr, ty)
            }
            ExprKind::AddrOf(k, m, ref ohs) => {
                let ohs = self.lower_expr(ohs);
                hir::ExprKind::AddrOf(k, m, ohs)
            }
            ExprKind::Let(ref pat, ref scrutinee) => self.lower_expr_let(e.span, pat, scrutinee),
            ExprKind::If(ref cond, ref then, ref else_opt) => {
                self.lower_expr_if(e.span, cond, then, else_opt.as_deref())
            }
            ExprKind::While(ref cond, ref body, opt_label) => self.with_loop_scope(e.id, |this| {
                this.lower_expr_while_in_loop_scope(e.span, cond, body, opt_label)
            }),
            ExprKind::Loop(ref body, opt_label) => self.with_loop_scope(e.id, |this| {
                hir::ExprKind::Loop(this.lower_block(body, false), opt_label, hir::LoopSource::Loop)
            }),
            ExprKind::TryBlock(ref body) => self.lower_expr_try_block(body),
            ExprKind::Match(ref expr, ref arms) => hir::ExprKind::Match(
                self.lower_expr(expr),
                self.arena.alloc_from_iter(arms.iter().map(|x| self.lower_arm(x))),
                hir::MatchSource::Normal,
            ),
            ExprKind::Async(capture_clause, closure_node_id, ref block) => self.make_async_expr(
                capture_clause,
                closure_node_id,
                None,
                block.span,
                hir::AsyncGeneratorKind::Block,
                |this| this.with_new_scopes(|this| this.lower_block_expr(block)),
            ),
            ExprKind::Await(ref expr) => self.lower_expr_await(e.span, expr),
            ExprKind::Closure(
                capture_clause,
                asyncness,
                movability,
                ref decl,
                ref body,
                fn_decl_span,
            ) => {
                if let Async::Yes { closure_id, .. } = asyncness {
                    self.lower_expr_async_closure(
                        capture_clause,
                        closure_id,
                        decl,
                        body,
                        fn_decl_span,
                    )
                } else {
                    self.lower_expr_closure(capture_clause, movability, decl, body, fn_decl_span)
                }
            }
            ExprKind::Block(ref blk, opt_label) => {
                hir::ExprKind::Block(self.lower_block(blk, opt_label.is_some()), opt_label)
            }
            ExprKind::Assign(ref el, ref er, span) => {
                hir::ExprKind::Assign(self.lower_expr(el), self.lower_expr(er), span)
            }
            ExprKind::AssignOp(op, ref el, ref er) => hir::ExprKind::AssignOp(
                self.lower_binop(op),
                self.lower_expr(el),
                self.lower_expr(er),
            ),
            ExprKind::Field(ref el, ident) => hir::ExprKind::Field(self.lower_expr(el), ident),
            ExprKind::Index(ref el, ref er) => {
                hir::ExprKind::Index(self.lower_expr(el), self.lower_expr(er))
            }
            ExprKind::Range(Some(ref e1), Some(ref e2), RangeLimits::Closed) => {
                self.lower_expr_range_closed(e.span, e1, e2)
            }
            ExprKind::Range(ref e1, ref e2, lims) => {
                self.lower_expr_range(e.span, e1.as_deref(), e2.as_deref(), lims)
            }
            ExprKind::Path(ref qself, ref path) => {
                let qpath = self.lower_qpath(
                    e.id,
                    qself,
                    path,
                    ParamMode::Optional,
                    ImplTraitContext::disallowed(),
                );
                hir::ExprKind::Path(qpath)
            }
            ExprKind::Break(opt_label, ref opt_expr) => {
                let opt_expr = opt_expr.as_ref().map(|x| self.lower_expr(x));
                hir::ExprKind::Break(self.lower_jump_destination(e.id, opt_label), opt_expr)
            }
            ExprKind::Continue(opt_label) => {
                hir::ExprKind::Continue(self.lower_jump_destination(e.id, opt_label))
            }
            ExprKind::Ret(ref e) => {
                let e = e.as_ref().map(|x| self.lower_expr(x));
                hir::ExprKind::Ret(e)
            }
            ExprKind::InlineAsm(ref asm) => self.lower_expr_asm(asm),
            ExprKind::Struct(ref path, ref fields, ref maybe_expr) => {
                let maybe_expr = maybe_expr.as_ref().map(|x| self.lower_expr(x));
                hir::ExprKind::Struct(
                    self.arena.alloc(self.lower_qpath(
                        e.id,
                        &None,
                        path,
                        ParamMode::Optional,
                        ImplTraitContext::disallowed(),
                    )),
                    self.arena.alloc_from_iter(fields.iter().map(|x| self.lower_field(x))),
                    maybe_expr,
                )
            }
            ExprKind::Paren(ref ex) => {
                let mut ex = self.lower_expr_mut(ex);
                // Include parens in span, but only if it is a super-span.
                if e.span.contains(ex.span) {
                    ex.span = e.span;
                }
                // Merge attributes into the inner expression.
                let mut attrs = e.attrs.clone();
                attrs.extend::<Vec<_>>(ex.attrs.into());
                ex.attrs = attrs;
                return ex;
            }

            ExprKind::Yield(ref opt_expr) => self.lower_expr_yield(e.span, opt_expr.as_deref()),

            ExprKind::Err => hir::ExprKind::Err,

            // Desugar `ExprForLoop`
            // from: `[opt_ident]: for <pat> in <head> <body>`
            ExprKind::ForLoop(ref pat, ref head, ref body, opt_label) => {
                return self.lower_expr_for(e, pat, head, body, opt_label);
            }
            ExprKind::Try(ref sub_expr) => self.lower_expr_try(e.span, sub_expr),
            ExprKind::MacCall(_) => panic!("Shouldn't exist here"),
        };

        hir::Expr {
            hir_id: self.lower_node_id(e.id),
            kind,
            span: e.span,
            attrs: e.attrs.iter().map(|a| self.lower_attr(a)).collect::<Vec<_>>().into(),
        }
    }

    fn lower_unop(&mut self, u: UnOp) -> hir::UnOp {
        match u {
            UnOp::Deref => hir::UnOp::UnDeref,
            UnOp::Not => hir::UnOp::UnNot,
            UnOp::Neg => hir::UnOp::UnNeg,
        }
    }

    fn lower_binop(&mut self, b: BinOp) -> hir::BinOp {
        Spanned {
            node: match b.node {
                BinOpKind::Add => hir::BinOpKind::Add,
                BinOpKind::Sub => hir::BinOpKind::Sub,
                BinOpKind::Mul => hir::BinOpKind::Mul,
                BinOpKind::Div => hir::BinOpKind::Div,
                BinOpKind::Rem => hir::BinOpKind::Rem,
                BinOpKind::And => hir::BinOpKind::And,
                BinOpKind::Or => hir::BinOpKind::Or,
                BinOpKind::BitXor => hir::BinOpKind::BitXor,
                BinOpKind::BitAnd => hir::BinOpKind::BitAnd,
                BinOpKind::BitOr => hir::BinOpKind::BitOr,
                BinOpKind::Shl => hir::BinOpKind::Shl,
                BinOpKind::Shr => hir::BinOpKind::Shr,
                BinOpKind::Eq => hir::BinOpKind::Eq,
                BinOpKind::Lt => hir::BinOpKind::Lt,
                BinOpKind::Le => hir::BinOpKind::Le,
                BinOpKind::Ne => hir::BinOpKind::Ne,
                BinOpKind::Ge => hir::BinOpKind::Ge,
                BinOpKind::Gt => hir::BinOpKind::Gt,
            },
            span: b.span,
        }
    }

    /// Emit an error and lower `ast::ExprKind::Let(pat, scrutinee)` into:
    /// ```rust
    /// match scrutinee { pats => true, _ => false }
    /// ```
    fn lower_expr_let(&mut self, span: Span, pat: &Pat, scrutinee: &Expr) -> hir::ExprKind<'hir> {
        // If we got here, the `let` expression is not allowed.

        if self.sess.opts.unstable_features.is_nightly_build() {
            self.sess
                .struct_span_err(span, "`let` expressions are not supported here")
                .note("only supported directly in conditions of `if`- and `while`-expressions")
                .note("as well as when nested within `&&` and parenthesis in those conditions")
                .emit();
        } else {
            self.sess
                .struct_span_err(span, "expected expression, found statement (`let`)")
                .note("variable declaration using `let` is a statement")
                .emit();
        }

        // For better recovery, we emit:
        // ```
        // match scrutinee { pat => true, _ => false }
        // ```
        // While this doesn't fully match the user's intent, it has key advantages:
        // 1. We can avoid using `abort_if_errors`.
        // 2. We can typeck both `pat` and `scrutinee`.
        // 3. `pat` is allowed to be refutable.
        // 4. The return type of the block is `bool` which seems like what the user wanted.
        let scrutinee = self.lower_expr(scrutinee);
        let then_arm = {
            let pat = self.lower_pat(pat);
            let expr = self.expr_bool(span, true);
            self.arm(pat, expr)
        };
        let else_arm = {
            let pat = self.pat_wild(span);
            let expr = self.expr_bool(span, false);
            self.arm(pat, expr)
        };
        hir::ExprKind::Match(
            scrutinee,
            arena_vec![self; then_arm, else_arm],
            hir::MatchSource::Normal,
        )
    }

    fn lower_expr_if(
        &mut self,
        span: Span,
        cond: &Expr,
        then: &Block,
        else_opt: Option<&Expr>,
    ) -> hir::ExprKind<'hir> {
        // FIXME(#53667): handle lowering of && and parens.

        // `_ => else_block` where `else_block` is `{}` if there's `None`:
        let else_pat = self.pat_wild(span);
        let (else_expr, contains_else_clause) = match else_opt {
            None => (self.expr_block_empty(span), false),
            Some(els) => (self.lower_expr(els), true),
        };
        let else_arm = self.arm(else_pat, else_expr);

        // Handle then + scrutinee:
        let then_expr = self.lower_block_expr(then);
        let (then_pat, scrutinee, desugar) = match cond.kind {
            // `<pat> => <then>`:
            ExprKind::Let(ref pat, ref scrutinee) => {
                let scrutinee = self.lower_expr(scrutinee);
                let pat = self.lower_pat(pat);
                (pat, scrutinee, hir::MatchSource::IfLetDesugar { contains_else_clause })
            }
            // `true => <then>`:
            _ => {
                // Lower condition:
                let cond = self.lower_expr(cond);
                let span_block =
                    self.mark_span_with_reason(DesugaringKind::CondTemporary, cond.span, None);
                // Wrap in a construct equivalent to `{ let _t = $cond; _t }`
                // to preserve drop semantics since `if cond { ... }` does not
                // let temporaries live outside of `cond`.
                let cond = self.expr_drop_temps(span_block, cond, ThinVec::new());
                let pat = self.pat_bool(span, true);
                (pat, cond, hir::MatchSource::IfDesugar { contains_else_clause })
            }
        };
        let then_arm = self.arm(then_pat, self.arena.alloc(then_expr));

        hir::ExprKind::Match(scrutinee, arena_vec![self; then_arm, else_arm], desugar)
    }

    fn lower_expr_while_in_loop_scope(
        &mut self,
        span: Span,
        cond: &Expr,
        body: &Block,
        opt_label: Option<Label>,
    ) -> hir::ExprKind<'hir> {
        // FIXME(#53667): handle lowering of && and parens.

        // Note that the block AND the condition are evaluated in the loop scope.
        // This is done to allow `break` from inside the condition of the loop.

        // `_ => break`:
        let else_arm = {
            let else_pat = self.pat_wild(span);
            let else_expr = self.expr_break(span, ThinVec::new());
            self.arm(else_pat, else_expr)
        };

        // Handle then + scrutinee:
        let then_expr = self.lower_block_expr(body);
        let (then_pat, scrutinee, desugar, source) = match cond.kind {
            ExprKind::Let(ref pat, ref scrutinee) => {
                // to:
                //
                //   [opt_ident]: loop {
                //     match <sub_expr> {
                //       <pat> => <body>,
                //       _ => break
                //     }
                //   }
                let scrutinee = self.with_loop_condition_scope(|t| t.lower_expr(scrutinee));
                let pat = self.lower_pat(pat);
                (pat, scrutinee, hir::MatchSource::WhileLetDesugar, hir::LoopSource::WhileLet)
            }
            _ => {
                // We desugar: `'label: while $cond $body` into:
                //
                // ```
                // 'label: loop {
                //     match drop-temps { $cond } {
                //         true => $body,
                //         _ => break,
                //     }
                // }
                // ```

                // Lower condition:
                let cond = self.with_loop_condition_scope(|this| this.lower_expr(cond));
                let span_block =
                    self.mark_span_with_reason(DesugaringKind::CondTemporary, cond.span, None);
                // Wrap in a construct equivalent to `{ let _t = $cond; _t }`
                // to preserve drop semantics since `while cond { ... }` does not
                // let temporaries live outside of `cond`.
                let cond = self.expr_drop_temps(span_block, cond, ThinVec::new());
                // `true => <then>`:
                let pat = self.pat_bool(span, true);
                (pat, cond, hir::MatchSource::WhileDesugar, hir::LoopSource::While)
            }
        };
        let then_arm = self.arm(then_pat, self.arena.alloc(then_expr));

        // `match <scrutinee> { ... }`
        let match_expr = self.expr_match(
            scrutinee.span,
            scrutinee,
            arena_vec![self; then_arm, else_arm],
            desugar,
        );

        // `[opt_ident]: loop { ... }`
        hir::ExprKind::Loop(self.block_expr(self.arena.alloc(match_expr)), opt_label, source)
    }

    /// Desugar `try { <stmts>; <expr> }` into `{ <stmts>; ::std::ops::Try::from_ok(<expr>) }`,
    /// `try { <stmts>; }` into `{ <stmts>; ::std::ops::Try::from_ok(()) }`
    /// and save the block id to use it as a break target for desugaring of the `?` operator.
    fn lower_expr_try_block(&mut self, body: &Block) -> hir::ExprKind<'hir> {
        self.with_catch_scope(body.id, |this| {
            let mut block = this.lower_block_noalloc(body, true);

            let try_span = this.mark_span_with_reason(
                DesugaringKind::TryBlock,
                body.span,
                this.allow_try_trait.clone(),
            );

            // Final expression of the block (if present) or `()` with span at the end of block
            let tail_expr = block
                .expr
                .take()
                .unwrap_or_else(|| this.expr_unit(this.sess.source_map().end_point(try_span)));

            let ok_wrapped_span =
                this.mark_span_with_reason(DesugaringKind::TryBlock, tail_expr.span, None);

            // `::std::ops::Try::from_ok($tail_expr)`
            block.expr = Some(this.wrap_in_try_constructor(
                sym::from_ok,
                try_span,
                tail_expr,
                ok_wrapped_span,
            ));

            hir::ExprKind::Block(this.arena.alloc(block), None)
        })
    }

    fn wrap_in_try_constructor(
        &mut self,
        method: Symbol,
        method_span: Span,
        expr: &'hir hir::Expr<'hir>,
        overall_span: Span,
    ) -> &'hir hir::Expr<'hir> {
        let path = &[sym::ops, sym::Try, method];
        let constructor =
            self.arena.alloc(self.expr_std_path(method_span, path, None, ThinVec::new()));
        self.expr_call(overall_span, constructor, std::slice::from_ref(expr))
    }

    fn lower_arm(&mut self, arm: &Arm) -> hir::Arm<'hir> {
        hir::Arm {
            hir_id: self.next_id(),
            attrs: self.lower_attrs(&arm.attrs),
            pat: self.lower_pat(&arm.pat),
            guard: match arm.guard {
                Some(ref x) => Some(hir::Guard::If(self.lower_expr(x))),
                _ => None,
            },
            body: self.lower_expr(&arm.body),
            span: arm.span,
        }
    }

    pub(super) fn make_async_expr(
        &mut self,
        capture_clause: CaptureBy,
        closure_node_id: NodeId,
        ret_ty: Option<AstP<Ty>>,
        span: Span,
        async_gen_kind: hir::AsyncGeneratorKind,
        body: impl FnOnce(&mut Self) -> hir::Expr<'hir>,
    ) -> hir::ExprKind<'hir> {
        let output = match ret_ty {
            Some(ty) => FnRetTy::Ty(ty),
            None => FnRetTy::Default(span),
        };

        let task_context_id = self.resolver.next_node_id();
        let task_context_hid = self.lower_node_id(task_context_id);

        let arg_ty = Ty { id: self.resolver.next_node_id(), kind: TyKind::Infer, span: DUMMY_SP };
        let arg_pat = Pat {
            id: task_context_id,
            kind: PatKind::Ident(
                BindingMode::ByValue(Mutability::Mut),
                Ident::with_dummy_span(sym::_task_context),
                None,
            ),
            span: DUMMY_SP,
        };
        let ast_decl = FnDecl {
            inputs: vec![Param {
                attrs: AttrVec::new(),
                ty: AstP(arg_ty),
                pat: AstP(arg_pat),
                id: self.resolver.next_node_id(),
                span: DUMMY_SP,
                is_placeholder: false,
            }],
            output,
        };
        let decl = self.lower_fn_decl(&ast_decl, None, /* impl trait allowed */ false, None);
        let body_id = self.lower_fn_body(&ast_decl, |this| {
            this.generator_kind = Some(hir::GeneratorKind::Async(async_gen_kind));

            let old_ctx = this.task_context;
            this.task_context = Some(task_context_hid);
            let res = body(this);
            this.task_context = old_ctx;
            res
        });

        // `static |task_context| -> <ret_ty> { body }`:
        let generator_kind = hir::ExprKind::Closure(
            capture_clause,
            decl,
            body_id,
            span,
            Some(hir::Movability::Static),
        );
        let generator = hir::Expr {
            hir_id: self.lower_node_id(closure_node_id),
            kind: generator_kind,
            span,
            attrs: ThinVec::new(),
        };

        // `future::from_generator`:
        let unstable_span =
            self.mark_span_with_reason(DesugaringKind::Async, span, self.allow_gen_future.clone());
        let gen_future = self.expr_std_path(
            unstable_span,
            &[sym::future, sym::from_generator],
            None,
            ThinVec::new(),
        );

        // `future::from_generator(generator)`:
        hir::ExprKind::Call(self.arena.alloc(gen_future), arena_vec![self; generator])
    }

    /// Desugar `<expr>.await` into:
    /// ```rust
    /// match <expr> {
    ///     mut pinned => loop {
    ///         match unsafe { ::std::future::poll_with_context(
    ///             <::std::pin::Pin>::new_unchecked(&mut pinned),
    ///             task_context,
    ///         ) } {
    ///             ::std::task::Poll::Ready(result) => break result,
    ///             ::std::task::Poll::Pending => {}
    ///         }
    ///         task_context = yield ();
    ///     }
    /// }
    /// ```
    fn lower_expr_await(&mut self, await_span: Span, expr: &Expr) -> hir::ExprKind<'hir> {
        match self.generator_kind {
            Some(hir::GeneratorKind::Async(_)) => {}
            Some(hir::GeneratorKind::Gen) | None => {
                let mut err = struct_span_err!(
                    self.sess,
                    await_span,
                    E0728,
                    "`await` is only allowed inside `async` functions and blocks"
                );
                err.span_label(await_span, "only allowed inside `async` functions and blocks");
                if let Some(item_sp) = self.current_item {
                    err.span_label(item_sp, "this is not `async`");
                }
                err.emit();
            }
        }
        let span = self.mark_span_with_reason(DesugaringKind::Await, await_span, None);
        let gen_future_span = self.mark_span_with_reason(
            DesugaringKind::Await,
            await_span,
            self.allow_gen_future.clone(),
        );

        let pinned_ident = Ident::with_dummy_span(sym::pinned);
        let (pinned_pat, pinned_pat_hid) =
            self.pat_ident_binding_mode(span, pinned_ident, hir::BindingAnnotation::Mutable);

        let task_context_ident = Ident::with_dummy_span(sym::_task_context);

        // unsafe {
        //     ::std::future::poll_with_context(
        //         ::std::pin::Pin::new_unchecked(&mut pinned),
        //         task_context,
        //     )
        // }
        let poll_expr = {
            let pinned = self.expr_ident(span, pinned_ident, pinned_pat_hid);
            let ref_mut_pinned = self.expr_mut_addr_of(span, pinned);
            let task_context = if let Some(task_context_hid) = self.task_context {
                self.expr_ident_mut(span, task_context_ident, task_context_hid)
            } else {
                // Use of `await` outside of an async context, we cannot use `task_context` here.
                self.expr_err(span)
            };
            let pin_ty_id = self.next_id();
            let new_unchecked_expr_kind = self.expr_call_std_assoc_fn(
                pin_ty_id,
                span,
                &[sym::pin, sym::Pin],
                "new_unchecked",
                arena_vec![self; ref_mut_pinned],
            );
            let new_unchecked = self.expr(span, new_unchecked_expr_kind, ThinVec::new());
            let call = self.expr_call_std_path(
                gen_future_span,
                &[sym::future, sym::poll_with_context],
                arena_vec![self; new_unchecked, task_context],
            );
            self.arena.alloc(self.expr_unsafe(call))
        };

        // `::std::task::Poll::Ready(result) => break result`
        let loop_node_id = self.resolver.next_node_id();
        let loop_hir_id = self.lower_node_id(loop_node_id);
        let ready_arm = {
            let x_ident = Ident::with_dummy_span(sym::result);
            let (x_pat, x_pat_hid) = self.pat_ident(span, x_ident);
            let x_expr = self.expr_ident(span, x_ident, x_pat_hid);
            let ready_pat = self.pat_std_enum(
                span,
                &[sym::task, sym::Poll, sym::Ready],
                arena_vec![self; x_pat],
            );
            let break_x = self.with_loop_scope(loop_node_id, move |this| {
                let expr_break =
                    hir::ExprKind::Break(this.lower_loop_destination(None), Some(x_expr));
                this.arena.alloc(this.expr(await_span, expr_break, ThinVec::new()))
            });
            self.arm(ready_pat, break_x)
        };

        // `::std::task::Poll::Pending => {}`
        let pending_arm = {
            let pending_pat = self.pat_std_enum(span, &[sym::task, sym::Poll, sym::Pending], &[]);
            let empty_block = self.expr_block_empty(span);
            self.arm(pending_pat, empty_block)
        };

        let inner_match_stmt = {
            let match_expr = self.expr_match(
                span,
                poll_expr,
                arena_vec![self; ready_arm, pending_arm],
                hir::MatchSource::AwaitDesugar,
            );
            self.stmt_expr(span, match_expr)
        };

        // task_context = yield ();
        let yield_stmt = {
            let unit = self.expr_unit(span);
            let yield_expr = self.expr(
                span,
                hir::ExprKind::Yield(unit, hir::YieldSource::Await),
                ThinVec::new(),
            );
            let yield_expr = self.arena.alloc(yield_expr);

            if let Some(task_context_hid) = self.task_context {
                let lhs = self.expr_ident(span, task_context_ident, task_context_hid);
                let assign =
                    self.expr(span, hir::ExprKind::Assign(lhs, yield_expr, span), AttrVec::new());
                self.stmt_expr(span, assign)
            } else {
                // Use of `await` outside of an async context. Return `yield_expr` so that we can
                // proceed with type checking.
                self.stmt(span, hir::StmtKind::Semi(yield_expr))
            }
        };

        let loop_block = self.block_all(span, arena_vec![self; inner_match_stmt, yield_stmt], None);

        // loop { .. }
        let loop_expr = self.arena.alloc(hir::Expr {
            hir_id: loop_hir_id,
            kind: hir::ExprKind::Loop(loop_block, None, hir::LoopSource::Loop),
            span,
            attrs: ThinVec::new(),
        });

        // mut pinned => loop { ... }
        let pinned_arm = self.arm(pinned_pat, loop_expr);

        // match <expr> {
        //     mut pinned => loop { .. }
        // }
        let expr = self.lower_expr(expr);
        hir::ExprKind::Match(expr, arena_vec![self; pinned_arm], hir::MatchSource::AwaitDesugar)
    }

    fn lower_expr_closure(
        &mut self,
        capture_clause: CaptureBy,
        movability: Movability,
        decl: &FnDecl,
        body: &Expr,
        fn_decl_span: Span,
    ) -> hir::ExprKind<'hir> {
        // Lower outside new scope to preserve `is_in_loop_condition`.
        let fn_decl = self.lower_fn_decl(decl, None, false, None);

        self.with_new_scopes(move |this| {
            let prev = this.current_item;
            this.current_item = Some(fn_decl_span);
            let mut generator_kind = None;
            let body_id = this.lower_fn_body(decl, |this| {
                let e = this.lower_expr_mut(body);
                generator_kind = this.generator_kind;
                e
            });
            let generator_option =
                this.generator_movability_for_fn(&decl, fn_decl_span, generator_kind, movability);
            this.current_item = prev;
            hir::ExprKind::Closure(capture_clause, fn_decl, body_id, fn_decl_span, generator_option)
        })
    }

    fn generator_movability_for_fn(
        &mut self,
        decl: &FnDecl,
        fn_decl_span: Span,
        generator_kind: Option<hir::GeneratorKind>,
        movability: Movability,
    ) -> Option<hir::Movability> {
        match generator_kind {
            Some(hir::GeneratorKind::Gen) => {
                if decl.inputs.len() > 1 {
                    struct_span_err!(
                        self.sess,
                        fn_decl_span,
                        E0628,
                        "too many parameters for a generator (expected 0 or 1 parameters)"
                    )
                    .emit();
                }
                Some(movability)
            }
            Some(hir::GeneratorKind::Async(_)) => {
                bug!("non-`async` closure body turned `async` during lowering");
            }
            None => {
                if movability == Movability::Static {
                    struct_span_err!(self.sess, fn_decl_span, E0697, "closures cannot be static")
                        .emit();
                }
                None
            }
        }
    }

    fn lower_expr_async_closure(
        &mut self,
        capture_clause: CaptureBy,
        closure_id: NodeId,
        decl: &FnDecl,
        body: &Expr,
        fn_decl_span: Span,
    ) -> hir::ExprKind<'hir> {
        let outer_decl =
            FnDecl { inputs: decl.inputs.clone(), output: FnRetTy::Default(fn_decl_span) };
        // We need to lower the declaration outside the new scope, because we
        // have to conserve the state of being inside a loop condition for the
        // closure argument types.
        let fn_decl = self.lower_fn_decl(&outer_decl, None, false, None);

        self.with_new_scopes(move |this| {
            // FIXME(cramertj): allow `async` non-`move` closures with arguments.
            if capture_clause == CaptureBy::Ref && !decl.inputs.is_empty() {
                struct_span_err!(
                    this.sess,
                    fn_decl_span,
                    E0708,
                    "`async` non-`move` closures with parameters are not currently supported",
                )
                .help(
                    "consider using `let` statements to manually capture \
                    variables by reference before entering an `async move` closure",
                )
                .emit();
            }

            // Transform `async |x: u8| -> X { ... }` into
            // `|x: u8| future_from_generator(|| -> X { ... })`.
            let body_id = this.lower_fn_body(&outer_decl, |this| {
                let async_ret_ty =
                    if let FnRetTy::Ty(ty) = &decl.output { Some(ty.clone()) } else { None };
                let async_body = this.make_async_expr(
                    capture_clause,
                    closure_id,
                    async_ret_ty,
                    body.span,
                    hir::AsyncGeneratorKind::Closure,
                    |this| this.with_new_scopes(|this| this.lower_expr_mut(body)),
                );
                this.expr(fn_decl_span, async_body, ThinVec::new())
            });
            hir::ExprKind::Closure(capture_clause, fn_decl, body_id, fn_decl_span, None)
        })
    }

    /// Desugar `<start>..=<end>` into `std::ops::RangeInclusive::new(<start>, <end>)`.
    fn lower_expr_range_closed(&mut self, span: Span, e1: &Expr, e2: &Expr) -> hir::ExprKind<'hir> {
        let id = self.next_id();
        let e1 = self.lower_expr_mut(e1);
        let e2 = self.lower_expr_mut(e2);
        self.expr_call_std_assoc_fn(
            id,
            span,
            &[sym::ops, sym::RangeInclusive],
            "new",
            arena_vec![self; e1, e2],
        )
    }

    fn lower_expr_range(
        &mut self,
        span: Span,
        e1: Option<&Expr>,
        e2: Option<&Expr>,
        lims: RangeLimits,
    ) -> hir::ExprKind<'hir> {
        use rustc_ast::ast::RangeLimits::*;

        let path = match (e1, e2, lims) {
            (None, None, HalfOpen) => sym::RangeFull,
            (Some(..), None, HalfOpen) => sym::RangeFrom,
            (None, Some(..), HalfOpen) => sym::RangeTo,
            (Some(..), Some(..), HalfOpen) => sym::Range,
            (None, Some(..), Closed) => sym::RangeToInclusive,
            (Some(..), Some(..), Closed) => unreachable!(),
            (_, None, Closed) => {
                self.diagnostic().span_fatal(span, "inclusive range with no end").raise()
            }
        };

        let fields = self.arena.alloc_from_iter(
            e1.iter().map(|e| ("start", e)).chain(e2.iter().map(|e| ("end", e))).map(|(s, e)| {
                let expr = self.lower_expr(&e);
                let ident = Ident::new(Symbol::intern(s), e.span);
                self.field(ident, expr, e.span)
            }),
        );

        let is_unit = fields.is_empty();
        let struct_path = [sym::ops, path];
        let struct_path = self.std_path(span, &struct_path, None, is_unit);
        let struct_path = hir::QPath::Resolved(None, struct_path);

        if is_unit {
            hir::ExprKind::Path(struct_path)
        } else {
            hir::ExprKind::Struct(self.arena.alloc(struct_path), fields, None)
        }
    }

    fn lower_loop_destination(&mut self, destination: Option<(NodeId, Label)>) -> hir::Destination {
        let target_id = match destination {
            Some((id, _)) => {
                if let Some(loop_id) = self.resolver.get_label_res(id) {
                    Ok(self.lower_node_id(loop_id))
                } else {
                    Err(hir::LoopIdError::UnresolvedLabel)
                }
            }
            None => self
                .loop_scopes
                .last()
                .cloned()
                .map(|id| Ok(self.lower_node_id(id)))
                .unwrap_or(Err(hir::LoopIdError::OutsideLoopScope)),
        };
        hir::Destination { label: destination.map(|(_, label)| label), target_id }
    }

    fn lower_jump_destination(&mut self, id: NodeId, opt_label: Option<Label>) -> hir::Destination {
        if self.is_in_loop_condition && opt_label.is_none() {
            hir::Destination {
                label: None,
                target_id: Err(hir::LoopIdError::UnlabeledCfInWhileCondition),
            }
        } else {
            self.lower_loop_destination(opt_label.map(|label| (id, label)))
        }
    }

    fn with_catch_scope<T>(&mut self, catch_id: NodeId, f: impl FnOnce(&mut Self) -> T) -> T {
        let len = self.catch_scopes.len();
        self.catch_scopes.push(catch_id);

        let result = f(self);
        assert_eq!(
            len + 1,
            self.catch_scopes.len(),
            "catch scopes should be added and removed in stack order"
        );

        self.catch_scopes.pop().unwrap();

        result
    }

    fn with_loop_scope<T>(&mut self, loop_id: NodeId, f: impl FnOnce(&mut Self) -> T) -> T {
        // We're no longer in the base loop's condition; we're in another loop.
        let was_in_loop_condition = self.is_in_loop_condition;
        self.is_in_loop_condition = false;

        let len = self.loop_scopes.len();
        self.loop_scopes.push(loop_id);

        let result = f(self);
        assert_eq!(
            len + 1,
            self.loop_scopes.len(),
            "loop scopes should be added and removed in stack order"
        );

        self.loop_scopes.pop().unwrap();

        self.is_in_loop_condition = was_in_loop_condition;

        result
    }

    fn with_loop_condition_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let was_in_loop_condition = self.is_in_loop_condition;
        self.is_in_loop_condition = true;

        let result = f(self);

        self.is_in_loop_condition = was_in_loop_condition;

        result
    }

    fn lower_expr_asm(&mut self, asm: &InlineAsm) -> hir::ExprKind<'hir> {
        let inner = hir::InlineAsmInner {
            inputs: asm.inputs.iter().map(|&(c, _)| c).collect(),
            outputs: asm
                .outputs
                .iter()
                .map(|out| hir::InlineAsmOutput {
                    constraint: out.constraint,
                    is_rw: out.is_rw,
                    is_indirect: out.is_indirect,
                    span: out.expr.span,
                })
                .collect(),
            asm: asm.asm,
            asm_str_style: asm.asm_str_style,
            clobbers: asm.clobbers.clone(),
            volatile: asm.volatile,
            alignstack: asm.alignstack,
            dialect: asm.dialect,
        };
        let hir_asm = hir::InlineAsm {
            inner,
            inputs_exprs: self.arena.alloc_from_iter(
                asm.inputs.iter().map(|&(_, ref input)| self.lower_expr_mut(input)),
            ),
            outputs_exprs: self
                .arena
                .alloc_from_iter(asm.outputs.iter().map(|out| self.lower_expr_mut(&out.expr))),
        };
        hir::ExprKind::InlineAsm(self.arena.alloc(hir_asm))
    }

    fn lower_field(&mut self, f: &Field) -> hir::Field<'hir> {
        hir::Field {
            hir_id: self.next_id(),
            ident: f.ident,
            expr: self.lower_expr(&f.expr),
            span: f.span,
            is_shorthand: f.is_shorthand,
        }
    }

    fn lower_expr_yield(&mut self, span: Span, opt_expr: Option<&Expr>) -> hir::ExprKind<'hir> {
        match self.generator_kind {
            Some(hir::GeneratorKind::Gen) => {}
            Some(hir::GeneratorKind::Async(_)) => {
                struct_span_err!(
                    self.sess,
                    span,
                    E0727,
                    "`async` generators are not yet supported"
                )
                .emit();
                return hir::ExprKind::Err;
            }
            None => self.generator_kind = Some(hir::GeneratorKind::Gen),
        }

        let expr =
            opt_expr.as_ref().map(|x| self.lower_expr(x)).unwrap_or_else(|| self.expr_unit(span));

        hir::ExprKind::Yield(expr, hir::YieldSource::Yield)
    }

    /// Desugar `ExprForLoop` from: `[opt_ident]: for <pat> in <head> <body>` into:
    /// ```rust
    /// {
    ///     let result = match ::std::iter::IntoIterator::into_iter(<head>) {
    ///         mut iter => {
    ///             [opt_ident]: loop {
    ///                 let mut __next;
    ///                 match ::std::iter::Iterator::next(&mut iter) {
    ///                     ::std::option::Option::Some(val) => __next = val,
    ///                     ::std::option::Option::None => break
    ///                 };
    ///                 let <pat> = __next;
    ///                 StmtKind::Expr(<body>);
    ///             }
    ///         }
    ///     };
    ///     result
    /// }
    /// ```
    fn lower_expr_for(
        &mut self,
        e: &Expr,
        pat: &Pat,
        head: &Expr,
        body: &Block,
        opt_label: Option<Label>,
    ) -> hir::Expr<'hir> {
        // expand <head>
        let mut head = self.lower_expr_mut(head);
        let desugared_span = self.mark_span_with_reason(DesugaringKind::ForLoop, head.span, None);
        head.span = desugared_span;

        let iter = Ident::with_dummy_span(sym::iter);

        let next_ident = Ident::with_dummy_span(sym::__next);
        let (next_pat, next_pat_hid) = self.pat_ident_binding_mode(
            desugared_span,
            next_ident,
            hir::BindingAnnotation::Mutable,
        );

        // `::std::option::Option::Some(val) => __next = val`
        let pat_arm = {
            let val_ident = Ident::with_dummy_span(sym::val);
            let (val_pat, val_pat_hid) = self.pat_ident(pat.span, val_ident);
            let val_expr = self.expr_ident(pat.span, val_ident, val_pat_hid);
            let next_expr = self.expr_ident(pat.span, next_ident, next_pat_hid);
            let assign = self.arena.alloc(self.expr(
                pat.span,
                hir::ExprKind::Assign(next_expr, val_expr, pat.span),
                ThinVec::new(),
            ));
            let some_pat = self.pat_some(pat.span, val_pat);
            self.arm(some_pat, assign)
        };

        // `::std::option::Option::None => break`
        let break_arm = {
            let break_expr =
                self.with_loop_scope(e.id, |this| this.expr_break(e.span, ThinVec::new()));
            let pat = self.pat_none(e.span);
            self.arm(pat, break_expr)
        };

        // `mut iter`
        let (iter_pat, iter_pat_nid) =
            self.pat_ident_binding_mode(desugared_span, iter, hir::BindingAnnotation::Mutable);

        // `match ::std::iter::Iterator::next(&mut iter) { ... }`
        let match_expr = {
            let iter = self.expr_ident(desugared_span, iter, iter_pat_nid);
            let ref_mut_iter = self.expr_mut_addr_of(desugared_span, iter);
            let next_path = &[sym::iter, sym::Iterator, sym::next];
            let next_expr =
                self.expr_call_std_path(desugared_span, next_path, arena_vec![self; ref_mut_iter]);
            let arms = arena_vec![self; pat_arm, break_arm];

            self.expr_match(desugared_span, next_expr, arms, hir::MatchSource::ForLoopDesugar)
        };
        let match_stmt = self.stmt_expr(desugared_span, match_expr);

        let next_expr = self.expr_ident(desugared_span, next_ident, next_pat_hid);

        // `let mut __next`
        let next_let = self.stmt_let_pat(
            ThinVec::new(),
            desugared_span,
            None,
            next_pat,
            hir::LocalSource::ForLoopDesugar,
        );

        // `let <pat> = __next`
        let pat = self.lower_pat(pat);
        let pat_let = self.stmt_let_pat(
            ThinVec::new(),
            desugared_span,
            Some(next_expr),
            pat,
            hir::LocalSource::ForLoopDesugar,
        );

        let body_block = self.with_loop_scope(e.id, |this| this.lower_block(body, false));
        let body_expr = self.expr_block(body_block, ThinVec::new());
        let body_stmt = self.stmt_expr(body.span, body_expr);

        let loop_block = self.block_all(
            e.span,
            arena_vec![self; next_let, match_stmt, pat_let, body_stmt],
            None,
        );

        // `[opt_ident]: loop { ... }`
        let kind = hir::ExprKind::Loop(loop_block, opt_label, hir::LoopSource::ForLoop);
        let loop_expr = self.arena.alloc(hir::Expr {
            hir_id: self.lower_node_id(e.id),
            kind,
            span: e.span,
            attrs: ThinVec::new(),
        });

        // `mut iter => { ... }`
        let iter_arm = self.arm(iter_pat, loop_expr);

        // `match ::std::iter::IntoIterator::into_iter(<head>) { ... }`
        let into_iter_expr = {
            let into_iter_path = &[sym::iter, sym::IntoIterator, sym::into_iter];
            self.expr_call_std_path(desugared_span, into_iter_path, arena_vec![self; head])
        };

        let match_expr = self.arena.alloc(self.expr_match(
            desugared_span,
            into_iter_expr,
            arena_vec![self; iter_arm],
            hir::MatchSource::ForLoopDesugar,
        ));

        // This is effectively `{ let _result = ...; _result }`.
        // The construct was introduced in #21984 and is necessary to make sure that
        // temporaries in the `head` expression are dropped and do not leak to the
        // surrounding scope of the `match` since the `match` is not a terminating scope.
        //
        // Also, add the attributes to the outer returned expr node.
        self.expr_drop_temps_mut(desugared_span, match_expr, e.attrs.clone())
    }

    /// Desugar `ExprKind::Try` from: `<expr>?` into:
    /// ```rust
    /// match Try::into_result(<expr>) {
    ///     Ok(val) => #[allow(unreachable_code)] val,
    ///     Err(err) => #[allow(unreachable_code)]
    ///                 // If there is an enclosing `try {...}`:
    ///                 break 'catch_target Try::from_error(From::from(err)),
    ///                 // Otherwise:
    ///                 return Try::from_error(From::from(err)),
    /// }
    /// ```
    fn lower_expr_try(&mut self, span: Span, sub_expr: &Expr) -> hir::ExprKind<'hir> {
        let unstable_span = self.mark_span_with_reason(
            DesugaringKind::QuestionMark,
            span,
            self.allow_try_trait.clone(),
        );
        let try_span = self.sess.source_map().end_point(span);
        let try_span = self.mark_span_with_reason(
            DesugaringKind::QuestionMark,
            try_span,
            self.allow_try_trait.clone(),
        );

        // `Try::into_result(<expr>)`
        let scrutinee = {
            // expand <expr>
            let sub_expr = self.lower_expr_mut(sub_expr);

            let path = &[sym::ops, sym::Try, sym::into_result];
            self.expr_call_std_path(unstable_span, path, arena_vec![self; sub_expr])
        };

        // `#[allow(unreachable_code)]`
        let attr = {
            // `allow(unreachable_code)`
            let allow = {
                let allow_ident = Ident::new(sym::allow, span);
                let uc_ident = Ident::new(sym::unreachable_code, span);
                let uc_nested = attr::mk_nested_word_item(uc_ident);
                attr::mk_list_item(allow_ident, vec![uc_nested])
            };
            attr::mk_attr_outer(allow)
        };
        let attrs = vec![attr];

        // `Ok(val) => #[allow(unreachable_code)] val,`
        let ok_arm = {
            let val_ident = Ident::with_dummy_span(sym::val);
            let (val_pat, val_pat_nid) = self.pat_ident(span, val_ident);
            let val_expr = self.arena.alloc(self.expr_ident_with_attrs(
                span,
                val_ident,
                val_pat_nid,
                ThinVec::from(attrs.clone()),
            ));
            let ok_pat = self.pat_ok(span, val_pat);
            self.arm(ok_pat, val_expr)
        };

        // `Err(err) => #[allow(unreachable_code)]
        //              return Try::from_error(From::from(err)),`
        let err_arm = {
            let err_ident = Ident::with_dummy_span(sym::err);
            let (err_local, err_local_nid) = self.pat_ident(try_span, err_ident);
            let from_expr = {
                let from_path = &[sym::convert, sym::From, sym::from];
                let err_expr = self.expr_ident_mut(try_span, err_ident, err_local_nid);
                self.expr_call_std_path(try_span, from_path, arena_vec![self; err_expr])
            };
            let from_err_expr =
                self.wrap_in_try_constructor(sym::from_error, unstable_span, from_expr, try_span);
            let thin_attrs = ThinVec::from(attrs);
            let catch_scope = self.catch_scopes.last().copied();
            let ret_expr = if let Some(catch_node) = catch_scope {
                let target_id = Ok(self.lower_node_id(catch_node));
                self.arena.alloc(self.expr(
                    try_span,
                    hir::ExprKind::Break(
                        hir::Destination { label: None, target_id },
                        Some(from_err_expr),
                    ),
                    thin_attrs,
                ))
            } else {
                self.arena.alloc(self.expr(
                    try_span,
                    hir::ExprKind::Ret(Some(from_err_expr)),
                    thin_attrs,
                ))
            };

            let err_pat = self.pat_err(try_span, err_local);
            self.arm(err_pat, ret_expr)
        };

        hir::ExprKind::Match(
            scrutinee,
            arena_vec![self; err_arm, ok_arm],
            hir::MatchSource::TryDesugar,
        )
    }

    // =========================================================================
    // Helper methods for building HIR.
    // =========================================================================

    /// Constructs a `true` or `false` literal expression.
    pub(super) fn expr_bool(&mut self, span: Span, val: bool) -> &'hir hir::Expr<'hir> {
        let lit = Spanned { span, node: LitKind::Bool(val) };
        self.arena.alloc(self.expr(span, hir::ExprKind::Lit(lit), ThinVec::new()))
    }

    /// Wrap the given `expr` in a terminating scope using `hir::ExprKind::DropTemps`.
    ///
    /// In terms of drop order, it has the same effect as wrapping `expr` in
    /// `{ let _t = $expr; _t }` but should provide better compile-time performance.
    ///
    /// The drop order can be important in e.g. `if expr { .. }`.
    pub(super) fn expr_drop_temps(
        &mut self,
        span: Span,
        expr: &'hir hir::Expr<'hir>,
        attrs: AttrVec,
    ) -> &'hir hir::Expr<'hir> {
        self.arena.alloc(self.expr_drop_temps_mut(span, expr, attrs))
    }

    pub(super) fn expr_drop_temps_mut(
        &mut self,
        span: Span,
        expr: &'hir hir::Expr<'hir>,
        attrs: AttrVec,
    ) -> hir::Expr<'hir> {
        self.expr(span, hir::ExprKind::DropTemps(expr), attrs)
    }

    fn expr_match(
        &mut self,
        span: Span,
        arg: &'hir hir::Expr<'hir>,
        arms: &'hir [hir::Arm<'hir>],
        source: hir::MatchSource,
    ) -> hir::Expr<'hir> {
        self.expr(span, hir::ExprKind::Match(arg, arms, source), ThinVec::new())
    }

    fn expr_break(&mut self, span: Span, attrs: AttrVec) -> &'hir hir::Expr<'hir> {
        let expr_break = hir::ExprKind::Break(self.lower_loop_destination(None), None);
        self.arena.alloc(self.expr(span, expr_break, attrs))
    }

    fn expr_mut_addr_of(&mut self, span: Span, e: &'hir hir::Expr<'hir>) -> hir::Expr<'hir> {
        self.expr(
            span,
            hir::ExprKind::AddrOf(hir::BorrowKind::Ref, hir::Mutability::Mut, e),
            ThinVec::new(),
        )
    }

    fn expr_unit(&mut self, sp: Span) -> &'hir hir::Expr<'hir> {
        self.arena.alloc(self.expr(sp, hir::ExprKind::Tup(&[]), ThinVec::new()))
    }

    fn expr_call(
        &mut self,
        span: Span,
        e: &'hir hir::Expr<'hir>,
        args: &'hir [hir::Expr<'hir>],
    ) -> &'hir hir::Expr<'hir> {
        self.arena.alloc(self.expr(span, hir::ExprKind::Call(e, args), ThinVec::new()))
    }

    // Note: associated functions must use `expr_call_std_path`.
    fn expr_call_std_path(
        &mut self,
        span: Span,
        path_components: &[Symbol],
        args: &'hir [hir::Expr<'hir>],
    ) -> &'hir hir::Expr<'hir> {
        let path =
            self.arena.alloc(self.expr_std_path(span, path_components, None, ThinVec::new()));
        self.expr_call(span, path, args)
    }

    // Create an expression calling an associated function of an std type.
    //
    // Associated functions cannot be resolved through the normal `std_path` function,
    // as they are resolved differently and so cannot use `expr_call_std_path`.
    //
    // This function accepts the path component (`ty_path_components`) separately from
    // the name of the associated function (`assoc_fn_name`) in order to facilitate
    // separate resolution of the type and creation of a path referring to its associated
    // function.
    fn expr_call_std_assoc_fn(
        &mut self,
        ty_path_id: hir::HirId,
        span: Span,
        ty_path_components: &[Symbol],
        assoc_fn_name: &str,
        args: &'hir [hir::Expr<'hir>],
    ) -> hir::ExprKind<'hir> {
        let ty_path = self.std_path(span, ty_path_components, None, false);
        let ty =
            self.arena.alloc(self.ty_path(ty_path_id, span, hir::QPath::Resolved(None, ty_path)));
        let fn_seg = self.arena.alloc(hir::PathSegment::from_ident(Ident::from_str(assoc_fn_name)));
        let fn_path = hir::QPath::TypeRelative(ty, fn_seg);
        let fn_expr =
            self.arena.alloc(self.expr(span, hir::ExprKind::Path(fn_path), ThinVec::new()));
        hir::ExprKind::Call(fn_expr, args)
    }

    fn expr_std_path(
        &mut self,
        span: Span,
        components: &[Symbol],
        params: Option<&'hir hir::GenericArgs<'hir>>,
        attrs: AttrVec,
    ) -> hir::Expr<'hir> {
        let path = self.std_path(span, components, params, true);
        self.expr(span, hir::ExprKind::Path(hir::QPath::Resolved(None, path)), attrs)
    }

    pub(super) fn expr_ident(
        &mut self,
        sp: Span,
        ident: Ident,
        binding: hir::HirId,
    ) -> &'hir hir::Expr<'hir> {
        self.arena.alloc(self.expr_ident_mut(sp, ident, binding))
    }

    pub(super) fn expr_ident_mut(
        &mut self,
        sp: Span,
        ident: Ident,
        binding: hir::HirId,
    ) -> hir::Expr<'hir> {
        self.expr_ident_with_attrs(sp, ident, binding, ThinVec::new())
    }

    fn expr_ident_with_attrs(
        &mut self,
        span: Span,
        ident: Ident,
        binding: hir::HirId,
        attrs: AttrVec,
    ) -> hir::Expr<'hir> {
        let expr_path = hir::ExprKind::Path(hir::QPath::Resolved(
            None,
            self.arena.alloc(hir::Path {
                span,
                res: Res::Local(binding),
                segments: arena_vec![self; hir::PathSegment::from_ident(ident)],
            }),
        ));

        self.expr(span, expr_path, attrs)
    }

    fn expr_unsafe(&mut self, expr: &'hir hir::Expr<'hir>) -> hir::Expr<'hir> {
        let hir_id = self.next_id();
        let span = expr.span;
        self.expr(
            span,
            hir::ExprKind::Block(
                self.arena.alloc(hir::Block {
                    stmts: &[],
                    expr: Some(expr),
                    hir_id,
                    rules: hir::BlockCheckMode::UnsafeBlock(hir::UnsafeSource::CompilerGenerated),
                    span,
                    targeted_by_break: false,
                }),
                None,
            ),
            ThinVec::new(),
        )
    }

    fn expr_block_empty(&mut self, span: Span) -> &'hir hir::Expr<'hir> {
        let blk = self.block_all(span, &[], None);
        let expr = self.expr_block(blk, ThinVec::new());
        self.arena.alloc(expr)
    }

    pub(super) fn expr_block(
        &mut self,
        b: &'hir hir::Block<'hir>,
        attrs: AttrVec,
    ) -> hir::Expr<'hir> {
        self.expr(b.span, hir::ExprKind::Block(b, None), attrs)
    }

    pub(super) fn expr(
        &mut self,
        span: Span,
        kind: hir::ExprKind<'hir>,
        attrs: AttrVec,
    ) -> hir::Expr<'hir> {
        hir::Expr { hir_id: self.next_id(), kind, span, attrs }
    }

    fn field(&mut self, ident: Ident, expr: &'hir hir::Expr<'hir>, span: Span) -> hir::Field<'hir> {
        hir::Field { hir_id: self.next_id(), ident, span, expr, is_shorthand: false }
    }

    fn arm(&mut self, pat: &'hir hir::Pat<'hir>, expr: &'hir hir::Expr<'hir>) -> hir::Arm<'hir> {
        hir::Arm {
            hir_id: self.next_id(),
            attrs: &[],
            pat,
            guard: None,
            span: expr.span,
            body: expr,
        }
    }
}
