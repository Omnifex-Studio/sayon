//! Recursive-descent parser with indentation-sensitive layout.
//!
//! Layout rules:
//! - A statement ends at the first token that starts a new physical line,
//!   unless that token is inside parentheses or brackets.
//! - A block is the run of lines indented deeper than the line that introduced
//!   it; every line of the block shares the first line's indentation.
//! - A function literal's body is a block even when the literal sits inside
//!   delimiters, so a callback can be passed as an indented argument.

use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::source::{FileId, Span};
use crate::token::{Keyword, StringPart, Token, TokenKind};

pub struct ParseOutput {
    pub module: Module,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn parse(file: FileId, tokens: &[Token]) -> ParseOutput {
    let mut parser = Parser::new(file, tokens, 0);
    let stmts = parser.parse_module();
    ParseOutput {
        module: Module {
            file,
            stmts,
            expr_count: parser.next_expr,
        },
        diagnostics: parser.diagnostics,
    }
}

/// Parses a whole source text. Lexer diagnostics come first.
pub fn parse_source(file: FileId, source: &str) -> ParseOutput {
    let lexed = crate::lexer::lex(file, source);
    let mut output = parse(file, &lexed.tokens);
    let mut diagnostics = lexed.diagnostics;
    diagnostics.append(&mut output.diagnostics);
    output.diagnostics = diagnostics;
    output
}

struct Parser<'t> {
    file: FileId,
    tokens: &'t [Token],
    pos: usize,
    /// Depth of open parentheses/brackets; line breaks are insignificant inside them.
    nesting: u32,
    /// Indentation of the line on which the current statement started.
    line_indent: u32,
    next_expr: u32,
    diagnostics: Vec<Diagnostic>,
    /// Set after an error until the parser resynchronises at a new line.
    panicking: bool,
    /// Position of the first token of the current line-level construct; that
    /// token starts a line but does not end the statement.
    line_head: usize,
}

type Result<T> = std::result::Result<T, ()>;

impl<'t> Parser<'t> {
    fn new(file: FileId, tokens: &'t [Token], next_expr: u32) -> Parser<'t> {
        Parser {
            file,
            tokens,
            pos: 0,
            nesting: 0,
            line_indent: 0,
            next_expr,
            diagnostics: Vec::new(),
            panicking: false,
            line_head: usize::MAX,
        }
    }

    // ----- token access -------------------------------------------------

    fn peek(&self) -> &'t Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, offset: usize) -> &'t Token {
        &self.tokens[(self.pos + offset).min(self.tokens.len() - 1)]
    }

    fn previous_span(&self) -> Span {
        if self.pos == 0 {
            self.peek().span
        } else {
            self.tokens[self.pos - 1].span
        }
    }

    fn at_eof(&self) -> bool {
        self.peek().kind == TokenKind::EndOfFile
    }

    /// True when the next token cannot continue the current statement.
    fn at_line_break(&self) -> bool {
        let token = self.peek();
        token.kind == TokenKind::EndOfFile
            || (self.nesting == 0 && token.line_start && self.pos != self.line_head)
    }

    fn check(&self, kind: &TokenKind) -> bool {
        !self.at_line_break() && &self.peek().kind == kind
    }

    fn check_keyword(&self, keyword: Keyword) -> bool {
        self.check(&TokenKind::Keyword(keyword))
    }

    fn advance(&mut self) -> &'t Token {
        let token = self.peek();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        token
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        self.eat(&TokenKind::Keyword(keyword))
    }

    fn found(&self) -> String {
        if self.at_line_break() && self.peek().kind != TokenKind::EndOfFile {
            "end of line".to_string()
        } else {
            self.peek().kind.describe()
        }
    }

    fn error_span(&self) -> Span {
        if self.at_line_break() && self.peek().kind != TokenKind::EndOfFile {
            self.previous_span().empty_at_end()
        } else {
            self.peek().span
        }
    }

    fn report(&mut self, diagnostic: Diagnostic) {
        if !self.panicking {
            self.diagnostics.push(diagnostic);
        }
        self.panicking = true;
    }

    fn error_expected(&mut self, what: &str) {
        let message = format!("expected {what}, found {}", self.found());
        let span = self.error_span();
        self.report(
            Diagnostic::error("E_SYNTAX", message).primary(span, format!("expected {what}")),
        );
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<Span> {
        if self.check(kind) {
            Ok(self.advance().span)
        } else {
            self.error_expected(what);
            Err(())
        }
    }

    fn expect_keyword(&mut self, keyword: Keyword) -> Result<Span> {
        self.expect(
            &TokenKind::Keyword(keyword),
            &format!("`{}`", keyword.as_str()),
        )
    }

    fn expect_ident(&mut self, what: &str) -> Result<Ident> {
        if !self.at_line_break()
            && let TokenKind::Identifier(name) = &self.peek().kind
        {
            let span = self.advance().span;
            return Ok(Ident {
                name: name.clone(),
                span,
            });
        }
        self.error_expected(what);
        Err(())
    }

    fn new_expr(&mut self, kind: ExprKind, span: Span) -> Expr {
        let id = ExprId(self.next_expr);
        self.next_expr += 1;
        Expr { id, kind, span }
    }

    fn error_expr(&mut self, span: Span) -> Expr {
        self.new_expr(ExprKind::Error, span)
    }

    /// Skips to the next line at the current statement's level, including any
    /// lines indented under the failed statement.
    fn recover(&mut self, statement_start: usize) {
        self.nesting = 0;
        if self.pos == statement_start && !self.at_eof() {
            self.advance();
        }
        while !self.at_eof() && !self.peek().line_start {
            self.advance();
        }
        while !self.at_eof() && self.peek().indent > self.line_indent {
            self.advance();
            while !self.at_eof() && !self.peek().line_start {
                self.advance();
            }
        }
        self.panicking = false;
    }

    fn finish_statement(&mut self, statement_start: usize) {
        if !self.panicking && !self.at_line_break() {
            let found = self.found();
            let span = self.peek().span;
            self.report(
                Diagnostic::error("E_SYNTAX", format!("expected end of line, found {found}"))
                    .primary(span, "unexpected here"),
            );
        }
        if self.panicking {
            self.recover(statement_start);
        }
    }

    // ----- layout --------------------------------------------------------

    fn parse_module(&mut self) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        while !self.at_eof() {
            let token = self.peek();
            let start = self.pos;
            self.line_indent = 0;
            if token.indent != 0 {
                self.report(
                    Diagnostic::error("E_UNEXPECTED_INDENT", "unexpected indentation").primary(
                        token.span,
                        "this line is indented but does not belong to a block",
                    ),
                );
                self.recover(start);
                continue;
            }
            self.line_indent = 0;
            self.line_head = self.pos;
            if let Ok(stmt) = self.parse_statement() {
                stmts.push(stmt);
            }
            self.finish_statement(start);
        }
        stmts
    }

    /// Parses an indented block following a header line.
    fn parse_block(&mut self, after: &str) -> Block {
        let saved_nesting = self.nesting;
        let saved_indent = self.line_indent;
        self.nesting = 0;
        let header_indent = self.line_indent;

        let first = self.peek();
        if !first.line_start || first.kind == TokenKind::EndOfFile || first.indent <= header_indent
        {
            let span = self.previous_span().empty_at_end();
            let message = format!("expected an indented block after {after}");
            if !first.line_start && first.kind != TokenKind::EndOfFile {
                let found = first.kind.describe();
                self.report(Diagnostic::error("E_EXPECTED_BLOCK", message).primary(
                    first.span,
                    format!("found {found}; the block starts on the next line"),
                ));
            } else {
                self.report(
                    Diagnostic::error("E_EXPECTED_BLOCK", message)
                        .primary(span, "add the block's lines below, indented by four spaces"),
                );
            }
            self.nesting = saved_nesting;
            self.line_indent = saved_indent;
            return Block {
                stmts: Vec::new(),
                span,
            };
        }

        let block_indent = first.indent;
        let block_start = first.span;
        let mut stmts = Vec::new();
        loop {
            let token = self.peek();
            if token.kind == TokenKind::EndOfFile || token.indent <= header_indent {
                break;
            }
            let start = self.pos;
            self.line_indent = block_indent;
            if token.indent < block_indent {
                self.report(
                    Diagnostic::error(
                        "E_INCONSISTENT_DEDENT",
                        "this line's indentation does not match any enclosing block",
                    )
                    .primary(token.span, "")
                    .secondary(block_start, "the block's lines start at this column"),
                );
                self.recover(start);
                continue;
            }
            if token.indent > block_indent {
                self.report(
                    Diagnostic::error("E_UNEXPECTED_INDENT", "unexpected indentation")
                        .primary(
                            token.span,
                            "this line is indented more than the lines before it",
                        )
                        .secondary(block_start, "the block's lines start at this column"),
                );
                self.recover(start);
                continue;
            }
            self.line_indent = block_indent;
            self.line_head = self.pos;
            if let Ok(stmt) = self.parse_statement() {
                stmts.push(stmt);
            }
            self.finish_statement(start);
        }

        let span = match stmts.last() {
            Some(last) => block_start.to(last.span),
            None => block_start,
        };
        self.nesting = saved_nesting;
        self.line_indent = saved_indent;
        Block { stmts, span }
    }

    /// True when the next token starts a continuation line (such as `else`)
    /// at the same indentation as the current statement.
    fn at_sibling_keyword(&self, keyword: Keyword) -> bool {
        let token = self.peek();
        token.line_start && token.indent == self.line_indent && token.is_keyword(keyword)
    }

    /// Skips lines indented under the current statement.
    fn skip_indented_lines(&mut self) {
        while !self.at_eof() && !(self.peek().line_start && self.peek().indent <= self.line_indent)
        {
            self.advance();
        }
    }

    // ----- statements -----------------------------------------------------

    fn parse_statement(&mut self) -> Result<Stmt> {
        let token = self.peek();
        let start = token.span;
        let kind = match &token.kind {
            TokenKind::Keyword(Keyword::Function)
                if matches!(self.peek_at(1).kind, TokenKind::Identifier(_)) =>
            {
                StmtKind::Function(Box::new(self.parse_function(Visibility::Default, true)?))
            }
            TokenKind::Keyword(Keyword::Public) | TokenKind::Keyword(Keyword::Private) => {
                let visibility = if token.is_keyword(Keyword::Public) {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                self.advance();
                self.parse_item_after_visibility(visibility)?
            }
            TokenKind::Keyword(Keyword::Variable) | TokenKind::Keyword(Keyword::Constant) => {
                StmtKind::Declare(self.parse_declaration(Visibility::Default)?)
            }
            TokenKind::Keyword(Keyword::If) => self.parse_if()?,
            TokenKind::Keyword(Keyword::While) => {
                self.advance();
                let condition = self.parse_expr()?;
                let body = self.parse_block("the `while` condition");
                StmtKind::While { condition, body }
            }
            TokenKind::Keyword(Keyword::For) => self.parse_for()?,
            TokenKind::Keyword(Keyword::Break) => {
                self.advance();
                StmtKind::Break
            }
            TokenKind::Keyword(Keyword::Continue) => {
                self.advance();
                StmtKind::Continue
            }
            TokenKind::Keyword(Keyword::Return) => {
                self.advance();
                if self.at_line_break() {
                    StmtKind::Return(None)
                } else {
                    StmtKind::Return(Some(self.parse_expr()?))
                }
            }
            TokenKind::Keyword(Keyword::Try) if self.peek_at(1).line_start => self.parse_try()?,
            TokenKind::Keyword(Keyword::Type) => self.parse_type_decl(Visibility::Default)?,
            TokenKind::Keyword(Keyword::Enum) => self.parse_enum_decl(Visibility::Default)?,
            TokenKind::Keyword(Keyword::Match) => self.parse_match()?,
            TokenKind::Keyword(Keyword::Foreign) => self.unavailable("foreign declarations", "M3"),
            TokenKind::Keyword(Keyword::Unsafe) => self.unavailable("unsafe blocks", "M3"),
            TokenKind::Keyword(Keyword::Async) | TokenKind::Keyword(Keyword::Await) => {
                self.unavailable("async and await", "M8")
            }
            TokenKind::Keyword(
                keyword @ (Keyword::Else | Keyword::Catch | Keyword::Finally | Keyword::Case),
            ) => {
                let (message, help) = match keyword {
                    Keyword::Else => (
                        "`else` without a matching `if`",
                        "`else` must line up with its `if`",
                    ),
                    Keyword::Case => (
                        "`case` outside a `match`",
                        "indent `case` lines under `match`",
                    ),
                    _ => (
                        "this clause has no matching `try`",
                        "`catch` and `finally` must line up with their `try`",
                    ),
                };
                self.report(
                    Diagnostic::error("E_SYNTAX", message)
                        .primary(start, "")
                        .help(help),
                );
                return Err(());
            }
            TokenKind::Identifier(_)
                if self.peek_at(1).kind == TokenKind::Colon && !self.peek_at(1).line_start =>
            {
                StmtKind::Declare(self.parse_annotated_declaration()?)
            }
            // `test "description"` is a test block; `test` is otherwise an ordinary name.
            TokenKind::Identifier(word)
                if word == "test"
                    && matches!(self.peek_at(1).kind, TokenKind::String(_))
                    && !self.peek_at(1).line_start =>
            {
                self.parse_test()?
            }
            _ => self.parse_expression_statement()?,
        };
        let span = start.to(self.previous_span());
        Ok(Stmt { kind, span })
    }

    fn unavailable(&mut self, feature: &'static str, milestone: &'static str) -> StmtKind {
        self.advance();
        self.skip_indented_lines();
        StmtKind::Unavailable { feature, milestone }
    }

    fn parse_item_after_visibility(&mut self, visibility: Visibility) -> Result<StmtKind> {
        let token = self.peek();
        match &token.kind {
            TokenKind::Keyword(Keyword::Function) => Ok(StmtKind::Function(Box::new(
                self.parse_function(visibility, true)?,
            ))),
            TokenKind::Keyword(Keyword::Constant) | TokenKind::Keyword(Keyword::Variable) => {
                Ok(StmtKind::Declare(self.parse_declaration(visibility)?))
            }
            TokenKind::Keyword(Keyword::Type) => self.parse_type_decl(visibility),
            TokenKind::Keyword(Keyword::Enum) => self.parse_enum_decl(visibility),
            _ => {
                self.error_expected("`function`, `constant`, `variable`, `type` or `enum`");
                Err(())
            }
        }
    }

    fn parse_declaration(&mut self, visibility: Visibility) -> Result<Declaration> {
        let kind = if self.eat_keyword(Keyword::Constant) {
            DeclKind::Constant
        } else {
            self.expect_keyword(Keyword::Variable)?;
            DeclKind::Variable
        };
        let name = self.expect_ident("a name")?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect_initializer(&name)?;
        let value = self.parse_expr()?;
        Ok(Declaration {
            kind,
            visibility,
            name,
            ty,
            value,
        })
    }

    fn parse_annotated_declaration(&mut self) -> Result<Declaration> {
        let name = self.expect_ident("a name")?;
        self.expect(&TokenKind::Colon, "`:`")?;
        let ty = self.parse_type()?;
        self.expect_initializer(&name)?;
        let value = self.parse_expr()?;
        Ok(Declaration {
            kind: DeclKind::Annotated,
            visibility: Visibility::Default,
            name,
            ty: Some(ty),
            value,
        })
    }

    fn expect_initializer(&mut self, name: &Ident) -> Result<()> {
        if self.eat(&TokenKind::Assign) {
            return Ok(());
        }
        if self.at_line_break() {
            let span = self.previous_span().empty_at_end();
            self.report(
                Diagnostic::error(
                    "E_UNINITIALIZED",
                    format!("`{}` must be given a value", name.name),
                )
                .primary(span, "expected `=` and an initial value")
                .help("every binding is initialized where it is declared"),
            );
            return Err(());
        }
        self.error_expected("`=`");
        Err(())
    }

    fn parse_expression_statement(&mut self) -> Result<StmtKind> {
        let target = self.parse_expr()?;
        let compound = match &self.peek().kind {
            TokenKind::PlusAssign => Some(BinaryOp::Add),
            TokenKind::MinusAssign => Some(BinaryOp::Subtract),
            TokenKind::StarAssign => Some(BinaryOp::Multiply),
            TokenKind::SlashAssign => Some(BinaryOp::Divide),
            TokenKind::PercentAssign => Some(BinaryOp::Remainder),
            _ => None,
        };
        if self.check(&TokenKind::Assign) {
            self.advance();
            self.check_assignment_target(&target);
            let value = self.parse_expr()?;
            return Ok(StmtKind::Assign { target, value });
        }
        if let Some(op) = compound.filter(|_| !self.at_line_break()) {
            let op_span = self.advance().span;
            self.check_assignment_target(&target);
            let value = self.parse_expr()?;
            return Ok(StmtKind::CompoundAssign {
                target,
                op,
                op_span,
                value,
            });
        }
        Ok(StmtKind::Expr(target))
    }

    fn check_assignment_target(&mut self, target: &Expr) {
        match &target.kind {
            ExprKind::Name(_)
            | ExprKind::Index { .. }
            | ExprKind::Member { .. }
            | ExprKind::Error => {}
            _ => self.report(
                Diagnostic::error("E_INVALID_ASSIGNMENT", "cannot assign to this expression")
                    .primary(target.span, "not a variable, element, or field")
                    .help("the left side of `=` must be a name, an indexed element, or a field"),
            ),
        }
    }

    fn parse_if(&mut self) -> Result<StmtKind> {
        self.expect_keyword(Keyword::If)?;
        let condition = self.parse_expr()?;
        let body = self.parse_block("the `if` condition");
        let mut branches = vec![IfBranch { condition, body }];
        let mut else_body = None;
        while self.at_sibling_keyword(Keyword::Else) {
            self.advance();
            if self.check_keyword(Keyword::If) {
                self.advance();
                let condition = self.parse_expr()?;
                let body = self.parse_block("the `else if` condition");
                branches.push(IfBranch { condition, body });
            } else {
                else_body = Some(self.parse_block("`else`"));
                break;
            }
        }
        Ok(StmtKind::If {
            branches,
            else_body,
        })
    }

    fn parse_for(&mut self) -> Result<StmtKind> {
        self.expect_keyword(Keyword::For)?;
        let mut bindings = vec![self.expect_ident("a loop variable name")?];
        while self.eat(&TokenKind::Comma) {
            bindings.push(self.expect_ident("a loop variable name")?);
        }
        self.expect_keyword(Keyword::In)?;
        let iterable = self.parse_expr()?;
        let body = self.parse_block("the `for` header");
        Ok(StmtKind::For {
            bindings,
            iterable,
            body,
        })
    }

    fn parse_try(&mut self) -> Result<StmtKind> {
        let try_span = self.expect_keyword(Keyword::Try)?;
        let body = self.parse_block("`try`");
        let mut catch = None;
        let mut finally = None;
        if self.at_sibling_keyword(Keyword::Catch) {
            self.advance();
            let binding = self.expect_ident("a name for the caught error")?;
            let body = self.parse_block("`catch`");
            catch = Some(CatchClause { binding, body });
        }
        if self.at_sibling_keyword(Keyword::Finally) {
            self.advance();
            finally = Some(self.parse_block("`finally`"));
        }
        if catch.is_none() && finally.is_none() {
            self.report(
                Diagnostic::error("E_SYNTAX", "`try` needs a `catch` or `finally` clause")
                    .primary(try_span, "")
                    .help("add `catch problem` or `finally` on a line aligned with `try`"),
            );
        }
        Ok(StmtKind::Try {
            body,
            catch,
            finally,
        })
    }

    fn parse_test(&mut self) -> Result<StmtKind> {
        self.advance();
        let token = self.advance();
        let name_span = token.span;
        let TokenKind::String(parts) = &token.kind else {
            unreachable!("checked by the caller")
        };
        let mut name = String::new();
        for part in parts {
            match part {
                StringPart::Text { text, .. } => name.push_str(text),
                StringPart::Interpolation { .. } => {
                    self.report(
                        Diagnostic::error(
                            "E_SYNTAX",
                            "a test's description cannot use interpolation",
                        )
                        .primary(name_span, "write the description as plain text"),
                    );
                    return Err(());
                }
            }
        }
        let body = self.parse_block("the test description");
        Ok(StmtKind::Test {
            name,
            name_span,
            body,
        })
    }

    fn parse_generic_params(&mut self) -> Result<Vec<Ident>> {
        let mut generics = Vec::new();
        if self.eat(&TokenKind::Less) {
            loop {
                generics.push(self.expect_ident("a type parameter name")?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::Greater, "`>`")?;
        }
        Ok(generics)
    }

    fn parse_function(&mut self, visibility: Visibility, named: bool) -> Result<Function> {
        let start = self.expect_keyword(Keyword::Function)?;
        let name = if named {
            Some(self.expect_ident("a function name")?)
        } else {
            None
        };
        let generics = self.parse_generic_params()?;
        self.expect(&TokenKind::LeftParen, "`(`")?;
        self.nesting += 1;
        let mut params = Vec::new();
        while !self.check(&TokenKind::RightParen) {
            params.push(self.parse_param()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen, "`)`")?;
        self.nesting -= 1;
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let header_span = start.to(self.previous_span());
        let body = self.parse_block("the function header");
        Ok(Function {
            visibility,
            name,
            generics,
            params,
            ret,
            body,
            header_span,
        })
    }

    fn parse_param(&mut self) -> Result<Param> {
        let start = self.peek().span;
        let mode = if self.check_keyword(Keyword::Own) {
            Some((ParamMode::Own, self.advance().span))
        } else if self.check_keyword(Keyword::Update) {
            Some((ParamMode::Update, self.advance().span))
        } else {
            None
        };
        let name = self.expect_ident("a parameter name")?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let default = if self.eat(&TokenKind::Assign) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Param {
            mode,
            name,
            ty,
            default,
            span: start.to(self.previous_span()),
        })
    }

    fn parse_fields(&mut self, close: TokenKind) -> Result<Vec<Field>> {
        let mut fields = Vec::new();
        while !self.check(&close) {
            let name = self.expect_ident("a field name")?;
            self.expect(&TokenKind::Colon, "`:`")?;
            let ty = self.parse_type()?;
            fields.push(Field { name, ty });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let what = format!("`{}`", close.symbol());
        self.expect(&close, &what)?;
        Ok(fields)
    }

    fn parse_type_decl(&mut self, visibility: Visibility) -> Result<StmtKind> {
        self.expect_keyword(Keyword::Type)?;
        let name = self.expect_ident("a type name")?;
        let generics = self.parse_generic_params()?;
        self.expect(&TokenKind::Assign, "`=`")?;
        self.expect(&TokenKind::LeftBracket, "`[`")?;
        self.nesting += 1;
        let fields = self.parse_fields(TokenKind::RightBracket)?;
        self.nesting -= 1;
        Ok(StmtKind::TypeDecl {
            visibility,
            name,
            generics,
            fields,
        })
    }

    fn parse_enum_decl(&mut self, visibility: Visibility) -> Result<StmtKind> {
        self.expect_keyword(Keyword::Enum)?;
        let name = self.expect_ident("an enum name")?;
        let generics = self.parse_generic_params()?;
        let header_indent = self.line_indent;
        let first = self.peek();
        if !first.line_start || first.indent <= header_indent || first.kind == TokenKind::EndOfFile
        {
            self.error_expected("indented variants on the following lines");
            return Err(());
        }
        let variant_indent = first.indent;
        let mut variants = Vec::new();
        while !self.at_eof() && self.peek().indent > header_indent {
            let start = self.pos;
            self.line_indent = variant_indent;
            if self.peek().indent != variant_indent {
                self.report(
                    Diagnostic::error("E_UNEXPECTED_INDENT", "enum variants must line up")
                        .primary(self.peek().span, ""),
                );
                self.recover(start);
                continue;
            }
            self.line_head = self.pos;
            let result = (|| -> Result<Variant> {
                let name = self.expect_ident("a variant name")?;
                let mut fields = Vec::new();
                if self.eat(&TokenKind::LeftParen) {
                    self.nesting += 1;
                    fields = self.parse_fields(TokenKind::RightParen)?;
                    self.nesting -= 1;
                }
                Ok(Variant { name, fields })
            })();
            if let Ok(variant) = result {
                variants.push(variant);
            }
            self.finish_statement(start);
        }
        self.line_indent = header_indent;
        Ok(StmtKind::EnumDecl {
            visibility,
            name,
            generics,
            variants,
        })
    }

    fn parse_match(&mut self) -> Result<StmtKind> {
        self.expect_keyword(Keyword::Match)?;
        let scrutinee = self.parse_expr()?;
        let header_indent = self.line_indent;
        let first = self.peek();
        if !first.line_start || first.indent <= header_indent || first.kind == TokenKind::EndOfFile
        {
            self.error_expected("indented `case` lines");
            return Err(());
        }
        let arm_indent = first.indent;
        let mut arms = Vec::new();
        while !self.at_eof() && self.peek().indent > header_indent {
            let start = self.pos;
            if self.peek().indent != arm_indent {
                self.report(
                    Diagnostic::error("E_UNEXPECTED_INDENT", "`case` lines must line up")
                        .primary(self.peek().span, ""),
                );
                self.recover(start);
                continue;
            }
            self.line_indent = arm_indent;
            self.line_head = self.pos;
            let result = (|| -> Result<MatchArm> {
                self.expect_keyword(Keyword::Case)?;
                let pattern = self.parse_pattern()?;
                let guard = if self.eat_keyword(Keyword::If) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                let body = self.parse_block("the `case` pattern");
                Ok(MatchArm {
                    pattern,
                    guard,
                    body,
                })
            })();
            if let Ok(arm) = result {
                arms.push(arm);
            }
            self.finish_statement(start);
        }
        self.line_indent = header_indent;
        Ok(StmtKind::Match { scrutinee, arms })
    }

    fn parse_pattern(&mut self) -> Result<Pattern> {
        let token = self.peek();
        let start = token.span;
        let kind = match &token.kind {
            TokenKind::Identifier(name) if name == "_" => {
                self.advance();
                PatternKind::Wildcard
            }
            TokenKind::Identifier(_) => {
                let name = self.expect_ident("a pattern")?;
                if self.eat(&TokenKind::LeftParen) {
                    self.nesting += 1;
                    let mut args = Vec::new();
                    while !self.check(&TokenKind::RightParen) {
                        args.push(self.parse_pattern()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RightParen, "`)`")?;
                    self.nesting -= 1;
                    PatternKind::Variant { name, args }
                } else {
                    PatternKind::Name(name)
                }
            }
            TokenKind::Integer { .. }
            | TokenKind::Float(_)
            | TokenKind::String(_)
            | TokenKind::Minus
            | TokenKind::Keyword(Keyword::True | Keyword::False | Keyword::Nothing) => {
                PatternKind::Literal(self.parse_unary()?)
            }
            _ => {
                self.error_expected("a pattern");
                return Err(());
            }
        };
        Ok(Pattern {
            kind,
            span: start.to(self.previous_span()),
        })
    }

    // ----- types ----------------------------------------------------------

    fn parse_type(&mut self) -> Result<TypeExpr> {
        let first = self.parse_type_atom()?;
        if !self.check_keyword(Keyword::Or) {
            return Ok(first);
        }
        let start = first.span;
        let mut members = vec![first];
        while self.eat_keyword(Keyword::Or) {
            members.push(self.parse_type_atom()?);
        }
        let span = start.to(self.previous_span());
        Ok(TypeExpr {
            kind: TypeExprKind::Union(members),
            span,
        })
    }

    fn parse_type_atom(&mut self) -> Result<TypeExpr> {
        if self.check_keyword(Keyword::Nothing) {
            let span = self.advance().span;
            let name = Ident {
                name: "nothing".to_string(),
                span,
            };
            return Ok(TypeExpr {
                kind: TypeExprKind::Named {
                    name,
                    args: Vec::new(),
                },
                span,
            });
        }
        if self.eat(&TokenKind::LeftParen) {
            self.nesting += 1;
            let inner = self.parse_type()?;
            self.expect(&TokenKind::RightParen, "`)`")?;
            self.nesting -= 1;
            return Ok(inner);
        }
        let name = self.expect_ident("a type")?;
        let mut args = Vec::new();
        if self.eat(&TokenKind::Less) {
            loop {
                args.push(self.parse_type()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::Greater, "`>`")?;
        }
        let span = name.span.to(self.previous_span());
        Ok(TypeExpr {
            kind: TypeExprKind::Named { name, args },
            span,
        })
    }

    /// Looks ahead from a `<` to decide whether it opens explicit generic
    /// arguments of a call such as `wrap<integer>(42)`.
    fn generic_call_ahead(&self) -> bool {
        let mut depth = 0usize;
        let mut index = self.pos;
        loop {
            let token = &self.tokens[index.min(self.tokens.len() - 1)];
            if token.kind == TokenKind::EndOfFile
                || (index > self.pos && self.nesting == 0 && token.line_start)
            {
                return false;
            }
            match &token.kind {
                TokenKind::Less => depth += 1,
                TokenKind::Greater => {
                    depth -= 1;
                    if depth == 0 {
                        let next = &self.tokens[(index + 1).min(self.tokens.len() - 1)];
                        return next.kind == TokenKind::LeftParen
                            && !(self.nesting == 0 && next.line_start);
                    }
                }
                // `or` is deliberately excluded so `a < b or c > (d)` stays a comparison.
                TokenKind::Identifier(_)
                | TokenKind::Comma
                | TokenKind::Keyword(Keyword::Nothing) => {}
                _ => return false,
            }
            index += 1;
        }
    }

    // ----- expressions ----------------------------------------------------

    pub(crate) fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn binary(&mut self, op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
        let span = lhs.span.to(rhs.span);
        self.new_expr(
            ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            span,
        )
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while self.eat_keyword(Keyword::Or) {
            let rhs = self.parse_and()?;
            lhs = self.binary(BinaryOp::Or, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_equality()?;
        while self.eat_keyword(Keyword::And) {
            let rhs = self.parse_equality()?;
            lhs = self.binary(BinaryOp::And, lhs, rhs);
        }
        Ok(lhs)
    }

    fn comparison_op(&self, ops: &[(TokenKind, BinaryOp)]) -> Option<BinaryOp> {
        if self.at_line_break() {
            return None;
        }
        ops.iter()
            .find(|(kind, _)| &self.peek().kind == kind)
            .map(|(_, op)| *op)
    }

    fn parse_equality(&mut self) -> Result<Expr> {
        let ops = [
            (TokenKind::Equal, BinaryOp::Equal),
            (TokenKind::NotEqual, BinaryOp::NotEqual),
        ];
        let mut lhs = self.parse_ordering()?;
        if self.check_keyword(Keyword::Is) {
            self.advance();
            let word = self.expect_ident("`error`")?;
            if word.name != "error" {
                self.report(
                    Diagnostic::error("E_SYNTAX", "`is` must be followed by `error`")
                        .primary(word.span, "")
                        .help("compare types with `typeof(value) == \"name\"`"),
                );
                return Err(());
            }
            let span = lhs.span.to(word.span);
            lhs = self.new_expr(ExprKind::IsError(Box::new(lhs)), span);
        }
        if let Some(op) = self.comparison_op(&ops) {
            self.advance();
            let rhs = self.parse_ordering()?;
            lhs = self.binary(op, lhs, rhs);
            if let Some(next) = self.comparison_op(&ops) {
                self.chained_comparison(op, next);
                return Err(());
            }
        }
        Ok(lhs)
    }

    fn parse_ordering(&mut self) -> Result<Expr> {
        let ops = [
            (TokenKind::Less, BinaryOp::Less),
            (TokenKind::Greater, BinaryOp::Greater),
            (TokenKind::LessEqual, BinaryOp::LessEqual),
            (TokenKind::GreaterEqual, BinaryOp::GreaterEqual),
        ];
        let mut lhs = self.parse_additive()?;
        if let Some(op) = self.comparison_op(&ops) {
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = self.binary(op, lhs, rhs);
            if let Some(next) = self.comparison_op(&ops) {
                self.chained_comparison(op, next);
                return Err(());
            }
        }
        Ok(lhs)
    }

    fn chained_comparison(&mut self, first: BinaryOp, second: BinaryOp) {
        let span = self.peek().span;
        self.report(
            Diagnostic::error("E_CHAINED_COMPARISON", "comparisons cannot be chained")
                .primary(span, format!("second comparison `{}`", second.symbol()))
                .help(format!(
                    "combine them with `and`, for example `a {} b and b {} c`",
                    first.symbol(),
                    second.symbol()
                )),
        );
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = if self.check(&TokenKind::Plus) {
                BinaryOp::Add
            } else if self.check(&TokenKind::Minus) {
                BinaryOp::Subtract
            } else {
                break;
            };
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = self.binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = if self.check(&TokenKind::Star) {
                BinaryOp::Multiply
            } else if self.check(&TokenKind::Slash) {
                BinaryOp::Divide
            } else if self.check(&TokenKind::Percent) {
                BinaryOp::Remainder
            } else {
                break;
            };
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = self.binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        let op = if self.check(&TokenKind::Minus) {
            UnaryOp::Negate
        } else if self.check_keyword(Keyword::Not) {
            UnaryOp::Not
        } else {
            return self.parse_postfix();
        };
        let start = self.advance().span;
        let operand = self.parse_unary()?;
        let span = start.to(operand.span);
        Ok(self.new_expr(
            ExprKind::Unary {
                op,
                operand: Box::new(operand),
            },
            span,
        ))
    }

    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.check(&TokenKind::LeftParen) {
                expr = self.finish_call(expr, Vec::new())?;
            } else if self.check(&TokenKind::Less)
                && matches!(expr.kind, ExprKind::Name(_) | ExprKind::Member { .. })
                && self.generic_call_ahead()
            {
                self.advance();
                let mut generic_args = Vec::new();
                loop {
                    generic_args.push(self.parse_type()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::Greater, "`>`")?;
                expr = self.finish_call(expr, generic_args)?;
            } else if self.check(&TokenKind::LeftBracket) {
                self.advance();
                self.nesting += 1;
                let index = self.parse_expr()?;
                let end = self.expect(&TokenKind::RightBracket, "`]`")?;
                self.nesting -= 1;
                let span = expr.span.to(end);
                expr = self.new_expr(
                    ExprKind::Index {
                        target: Box::new(expr),
                        index: Box::new(index),
                    },
                    span,
                );
            } else if self.check(&TokenKind::Dot) {
                self.advance();
                let name = self.expect_ident("a field or member name")?;
                let span = expr.span.to(name.span);
                expr = self.new_expr(
                    ExprKind::Member {
                        target: Box::new(expr),
                        name,
                    },
                    span,
                );
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn finish_call(&mut self, callee: Expr, generic_args: Vec<TypeExpr>) -> Result<Expr> {
        self.expect(&TokenKind::LeftParen, "`(`")?;
        self.nesting += 1;
        let mut args = Vec::new();
        while !self.check(&TokenKind::RightParen) {
            args.push(self.parse_expr()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect(&TokenKind::RightParen, "`)` or `,`")?;
        self.nesting -= 1;
        let span = callee.span.to(end);
        Ok(self.new_expr(
            ExprKind::Call {
                callee: Box::new(callee),
                generic_args,
                args,
            },
            span,
        ))
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        if self.at_line_break() {
            self.error_expected("an expression");
            return Err(());
        }
        let token = self.peek();
        let span = token.span;
        let kind = match &token.kind {
            TokenKind::Integer { value, unsigned } => {
                self.advance();
                ExprKind::Integer {
                    value: *value,
                    unsigned: *unsigned,
                }
            }
            TokenKind::Float(value) => {
                self.advance();
                ExprKind::Float(*value)
            }
            TokenKind::String(parts) => {
                self.advance();
                ExprKind::String(self.string_segments(parts, span))
            }
            TokenKind::Keyword(Keyword::True) => {
                self.advance();
                ExprKind::Boolean(true)
            }
            TokenKind::Keyword(Keyword::False) => {
                self.advance();
                ExprKind::Boolean(false)
            }
            TokenKind::Keyword(Keyword::Nothing) => {
                self.advance();
                ExprKind::Nothing
            }
            TokenKind::Identifier(name) => {
                self.advance();
                ExprKind::Name(Ident {
                    name: name.clone(),
                    span,
                })
            }
            TokenKind::LeftParen => {
                self.advance();
                self.nesting += 1;
                let inner = self.parse_expr()?;
                self.expect(&TokenKind::RightParen, "`)`")?;
                self.nesting -= 1;
                return Ok(inner);
            }
            TokenKind::LeftBracket => return self.parse_collection(),
            TokenKind::Keyword(Keyword::Function) => {
                let function = self.parse_function(Visibility::Default, false)?;
                let span = function.header_span.to(function.body.span);
                return Ok(self.new_expr(ExprKind::Function(Box::new(function)), span));
            }
            TokenKind::Keyword(Keyword::Try) => {
                self.advance();
                let operand = self.parse_unary()?;
                let span = span.to(operand.span);
                return Ok(self.new_expr(ExprKind::TryExpr(Box::new(operand)), span));
            }
            TokenKind::Keyword(Keyword::Import) => return self.parse_import(),
            _ => {
                self.error_expected("an expression");
                return Err(());
            }
        };
        Ok(self.new_expr(kind, span))
    }

    fn parse_import(&mut self) -> Result<Expr> {
        let start = self.expect_keyword(Keyword::Import)?;
        self.expect(&TokenKind::LeftParen, "`(`")?;
        self.nesting += 1;
        let token = self.peek();
        let path = match &token.kind {
            TokenKind::String(parts)
                if parts.iter().all(|p| matches!(p, StringPart::Text { .. })) =>
            {
                let text: String = parts
                    .iter()
                    .map(|part| match part {
                        StringPart::Text { text, .. } => text.as_str(),
                        StringPart::Interpolation { .. } => "",
                    })
                    .collect();
                self.advance();
                text
            }
            _ => {
                let span = token.span;
                self.report(
                    Diagnostic::error("E_IMPORT_PATH", "imports take a literal path string")
                        .primary(span, "expected a string without interpolation"),
                );
                return Err(());
            }
        };
        let path_span = token.span;
        let end = self.expect(&TokenKind::RightParen, "`)`")?;
        self.nesting -= 1;
        Ok(self.new_expr(ExprKind::Import { path, path_span }, start.to(end)))
    }

    fn parse_collection(&mut self) -> Result<Expr> {
        let start = self.expect(&TokenKind::LeftBracket, "`[`")?;
        self.nesting += 1;
        if self.check(&TokenKind::RightBracket) {
            let end = self.advance().span;
            self.nesting -= 1;
            return Ok(self.new_expr(ExprKind::Array(Vec::new()), start.to(end)));
        }
        let first = self.parse_expr()?;
        let kind = if self.eat(&TokenKind::Assign) {
            let value = self.parse_expr()?;
            if let ExprKind::Name(name) = &first.kind {
                let mut fields = vec![(name.clone(), value)];
                while self.eat(&TokenKind::Comma) {
                    if self.check(&TokenKind::RightBracket) {
                        break;
                    }
                    let name = self.expect_ident("a field name")?;
                    self.expect(&TokenKind::Assign, "`=`")?;
                    fields.push((name, self.parse_expr()?));
                }
                ExprKind::Record(fields)
            } else {
                let mut entries = vec![(first, value)];
                while self.eat(&TokenKind::Comma) {
                    if self.check(&TokenKind::RightBracket) {
                        break;
                    }
                    let key = self.parse_expr()?;
                    self.expect(&TokenKind::Assign, "`=` between key and value")?;
                    entries.push((key, self.parse_expr()?));
                }
                ExprKind::Dict(entries)
            }
        } else {
            let mut items = vec![first];
            while self.eat(&TokenKind::Comma) {
                if self.check(&TokenKind::RightBracket) {
                    break;
                }
                items.push(self.parse_expr()?);
            }
            ExprKind::Array(items)
        };
        let end = self.expect(&TokenKind::RightBracket, "`]` or `,`")?;
        self.nesting -= 1;
        Ok(self.new_expr(kind, start.to(end)))
    }

    fn string_segments(&mut self, parts: &[StringPart], literal_span: Span) -> Vec<StringSegment> {
        let mut segments = Vec::new();
        for part in parts {
            match part {
                StringPart::Text { text, .. } => segments.push(StringSegment::Text(text.clone())),
                StringPart::Interpolation { tokens, span } => {
                    let mut owned = tokens.clone();
                    owned.push(Token {
                        kind: TokenKind::EndOfFile,
                        span: Span {
                            file: self.file,
                            start: span.end - 1,
                            end: span.end - 1,
                        },
                        line_start: true,
                        indent: 0,
                    });
                    let mut inner = Parser::new(self.file, &owned, self.next_expr);
                    inner.nesting = 1;
                    let expr = match inner.parse_expr() {
                        Ok(expr) if inner.at_eof() => Some(expr),
                        Ok(_) => {
                            let span = inner.peek().span;
                            inner.report(
                                Diagnostic::error("E_SYNTAX", "unexpected token in interpolation")
                                    .primary(span, "expected `}` here"),
                            );
                            None
                        }
                        Err(()) => None,
                    };
                    self.next_expr = inner.next_expr;
                    self.diagnostics.append(&mut inner.diagnostics);
                    let expr = expr.unwrap_or_else(|| self.error_expr(*span));
                    segments.push(StringSegment::Interpolation(expr));
                }
            }
        }
        let _ = literal_span;
        segments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(source: &str) -> Module {
        let output = parse_source(FileId(0), source);
        assert!(
            output.diagnostics.is_empty(),
            "unexpected diagnostics: {:#?}",
            output.diagnostics
        );
        output.module
    }

    fn error_codes(source: &str) -> Vec<&'static str> {
        parse_source(FileId(0), source)
            .diagnostics
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    /// Renders expressions as fully parenthesised text for precedence tests.
    fn show(expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::Integer { value, .. } => value.to_string(),
            ExprKind::Float(value) => value.to_string(),
            ExprKind::Boolean(value) => value.to_string(),
            ExprKind::Nothing => "nothing".into(),
            ExprKind::Name(name) => name.name.clone(),
            ExprKind::String(segments) => {
                let inner: Vec<String> = segments
                    .iter()
                    .map(|segment| match segment {
                        StringSegment::Text(text) => text.clone(),
                        StringSegment::Interpolation(expr) => format!("{{{}}}", show(expr)),
                    })
                    .collect();
                format!("\"{}\"", inner.join(""))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                format!("({} {} {})", show(lhs), op.symbol(), show(rhs))
            }
            ExprKind::Unary {
                op: UnaryOp::Negate,
                operand,
            } => format!("(-{})", show(operand)),
            ExprKind::Unary {
                op: UnaryOp::Not,
                operand,
            } => format!("(not {})", show(operand)),
            ExprKind::Call {
                callee,
                generic_args,
                args,
            } => {
                let args: Vec<String> = args.iter().map(show).collect();
                let generics = if generic_args.is_empty() {
                    String::new()
                } else {
                    format!("<{}>", generic_args.len())
                };
                format!("{}{}({})", show(callee), generics, args.join(", "))
            }
            ExprKind::Index { target, index } => format!("{}[{}]", show(target), show(index)),
            ExprKind::Member { target, name } => format!("{}.{}", show(target), name.name),
            ExprKind::Array(items) => format!(
                "[{}]",
                items.iter().map(show).collect::<Vec<_>>().join(", ")
            ),
            ExprKind::Dict(entries) => format!(
                "[{}]",
                entries
                    .iter()
                    .map(|(k, v)| format!("{} = {}", show(k), show(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ExprKind::Record(fields) => format!(
                "record[{}]",
                fields
                    .iter()
                    .map(|(k, v)| format!("{} = {}", k.name, show(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ExprKind::IsError(inner) => format!("({} is error)", show(inner)),
            ExprKind::TryExpr(inner) => format!("(try {})", show(inner)),
            ExprKind::Function(_) => "function".into(),
            ExprKind::Import { path, .. } => format!("import({path})"),
            ExprKind::Error => "<error>".into(),
        }
    }

    fn expr_of(source: &str) -> String {
        let module = parse_ok(source);
        match &module.stmts[0].kind {
            StmtKind::Expr(expr) => show(expr),
            StmtKind::Assign { value, .. } => show(value),
            other => panic!("expected expression statement, got {other:?}"),
        }
    }

    #[test]
    fn precedence() {
        assert_eq!(expr_of("1 + 2 * 3"), "(1 + (2 * 3))");
        assert_eq!(expr_of("1 - 2 - 3"), "((1 - 2) - 3)");
        assert_eq!(expr_of("-a * b"), "((-a) * b)");
        assert_eq!(expr_of("not a and b or c"), "(((not a) and b) or c)");
        assert_eq!(expr_of("a < b == c > d"), "((a < b) == (c > d))");
        assert_eq!(
            expr_of("a + b < c * d and e != f"),
            "(((a + b) < (c * d)) and (e != f))"
        );
        assert_eq!(expr_of("f(x)[0].y"), "f(x)[0].y");
        assert_eq!(expr_of("- - 5"), "(-(-5))");
    }

    #[test]
    fn chained_comparisons_are_rejected() {
        assert_eq!(error_codes("a < b < c"), vec!["E_CHAINED_COMPARISON"]);
        assert_eq!(error_codes("a == b == c"), vec!["E_CHAINED_COMPARISON"]);
    }

    #[test]
    fn collections() {
        assert_eq!(expr_of("x = [1, 2, 3]"), "[1, 2, 3]");
        assert_eq!(expr_of("x = []"), "[]");
        assert_eq!(
            expr_of(r#"x = ["name" = "Alice", "age" = 30]"#),
            r#"["name" = "Alice", "age" = 30]"#
        );
        assert_eq!(
            expr_of("x = [x = 0.5, y = 1.5]"),
            "record[x = 0.5, y = 1.5]"
        );
        assert_eq!(expr_of("x = [\n    1,\n    2,\n]"), "[1, 2]");
    }

    #[test]
    fn interpolation_parses_expressions() {
        assert_eq!(
            expr_of(r#"print("Sum: {100 + 20}!")"#),
            r#"print("Sum: {(100 + 20)}!")"#
        );
        assert_eq!(expr_of(r#"print("{a["k"]}")"#), r#"print("{a["k"]}")"#);
        assert_eq!(error_codes(r#"print("{a b}")"#), vec!["E_SYNTAX"]);
    }

    #[test]
    fn generic_calls_and_comparisons() {
        assert_eq!(expr_of("wrap<integer>(42)"), "wrap<1>(42)");
        assert_eq!(expr_of("x = a < b"), "(a < b)");
        assert_eq!(expr_of("x = a < b and c > (d)"), "((a < b) and (c > d))");
    }

    #[test]
    fn statements_and_blocks() {
        let module = parse_ok(
            "count = 0\nwhile count < 3\n    count += 1\n    if count == 2\n        continue\nprint(count)\n",
        );
        assert_eq!(module.stmts.len(), 3);
        let StmtKind::While { body, .. } = &module.stmts[1].kind else {
            panic!()
        };
        assert_eq!(body.stmts.len(), 2);
        assert!(matches!(
            body.stmts[0].kind,
            StmtKind::CompoundAssign {
                op: BinaryOp::Add,
                ..
            }
        ));
    }

    #[test]
    fn if_else_chain() {
        let module = parse_ok("if a\n    x()\nelse if b\n    y()\nelse\n    z()\n");
        let StmtKind::If {
            branches,
            else_body,
        } = &module.stmts[0].kind
        else {
            panic!()
        };
        assert_eq!(branches.len(), 2);
        assert!(else_body.is_some());
    }

    #[test]
    fn functions_with_modes_defaults_and_types() {
        let module = parse_ok(
            "function append_score(update scores: Array<integer>, score: integer = 1) -> nothing\n    push(scores, score)\n\nfunction keep(own file: File) -> File\n    return file\n",
        );
        let StmtKind::Function(function) = &module.stmts[0].kind else {
            panic!()
        };
        assert_eq!(function.params.len(), 2);
        assert_eq!(
            function.params[0].mode.map(|m| m.0),
            Some(ParamMode::Update)
        );
        assert!(function.params[1].default.is_some());
        let StmtKind::Function(keep) = &module.stmts[1].kind else {
            panic!()
        };
        assert_eq!(keep.params[0].mode.map(|m| m.0), Some(ParamMode::Own));
    }

    #[test]
    fn declarations() {
        let module = parse_ok(
            "constant MAX_SIZE = 100\nvariable total = 0\npositive: unsigned = 255u\nresult: integer or nothing = find(1)\n",
        );
        let kinds: Vec<DeclKind> = module
            .stmts
            .iter()
            .map(|stmt| match &stmt.kind {
                StmtKind::Declare(decl) => decl.kind,
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                DeclKind::Constant,
                DeclKind::Variable,
                DeclKind::Annotated,
                DeclKind::Annotated
            ]
        );
        let StmtKind::Declare(decl) = &module.stmts[3].kind else {
            panic!()
        };
        assert!(
            matches!(decl.ty.as_ref().unwrap().kind, TypeExprKind::Union(ref members) if members.len() == 2)
        );
    }

    #[test]
    fn try_catch_finally() {
        let module = parse_ok(
            "try\n    value = divide(10, 0)\ncatch problem\n    print(\"Error: {problem}\")\nfinally\n    print(\"done\")\n",
        );
        let StmtKind::Try { catch, finally, .. } = &module.stmts[0].kind else {
            panic!()
        };
        assert_eq!(catch.as_ref().unwrap().binding.name, "problem");
        assert!(finally.is_some());
        assert_eq!(error_codes("try\n    x()\n"), vec!["E_SYNTAX"]);
    }

    #[test]
    fn callback_block_inside_call_arguments() {
        let module = parse_ok(
            "handle = spawn(function()\n    print(\"Running\")\n    return 42\n)\nresult = join(handle)\n",
        );
        assert_eq!(module.stmts.len(), 2);
        let StmtKind::Assign { value, .. } = &module.stmts[0].kind else {
            panic!()
        };
        let ExprKind::Call { args, .. } = &value.kind else {
            panic!()
        };
        let ExprKind::Function(function) = &args[0].kind else {
            panic!()
        };
        assert_eq!(function.body.stmts.len(), 2);
    }

    #[test]
    fn closures_returned_from_functions() {
        let module = parse_ok(
            "function make_counter()\n    count = 0\n    return function()\n        count = count + 1\n        return count\n\ncounter = make_counter()\n",
        );
        assert_eq!(module.stmts.len(), 2);
    }

    #[test]
    fn types_enums_and_match() {
        let module = parse_ok(
            "type Point = [\n    x: float,\n    y: float\n]\n\nenum Shape\n    circle(radius: float)\n    rectangle(width: float, height: float)\n    point\n\nmatch shape\n    case circle(radius)\n        print(radius)\n    case point\n        print(\"Point\")\n    case _\n        print(\"other\")\n",
        );
        assert_eq!(module.stmts.len(), 3);
        let StmtKind::EnumDecl { variants, .. } = &module.stmts[1].kind else {
            panic!()
        };
        assert_eq!(variants.len(), 3);
        assert_eq!(variants[1].fields.len(), 2);
        let StmtKind::Match { arms, .. } = &module.stmts[2].kind else {
            panic!()
        };
        assert_eq!(arms.len(), 3);
        assert!(matches!(arms[2].pattern.kind, PatternKind::Wildcard));
    }

    #[test]
    fn imports() {
        let module = parse_ok("math = import(\"src/math_utils.sayon\")\nprint(math.add(10, 20))\n");
        let StmtKind::Assign { value, .. } = &module.stmts[0].kind else {
            panic!()
        };
        assert!(
            matches!(&value.kind, ExprKind::Import { path, .. } if path == "src/math_utils.sayon")
        );
        assert_eq!(
            error_codes("m = import(\"{x}.sayon\")"),
            vec!["E_IMPORT_PATH"]
        );
    }

    #[test]
    fn expression_error_forms() {
        assert_eq!(expr_of("result = try read(\"x\")"), "(try read(\"x\"))");
        assert_eq!(expr_of("result is error"), "(result is error)");
    }

    #[test]
    fn layout_errors() {
        assert_eq!(error_codes("    x = 1\n"), vec!["E_UNEXPECTED_INDENT"]);
        assert_eq!(error_codes("if x\ny()\n"), vec!["E_EXPECTED_BLOCK"]);
        assert_eq!(
            error_codes("if x\n        a()\n    b()\n"),
            vec!["E_INCONSISTENT_DEDENT"]
        );
        assert_eq!(
            error_codes("if x\n    a()\n        b()\n"),
            vec!["E_UNEXPECTED_INDENT"]
        );
        assert_eq!(error_codes("else\n    x()\n"), vec!["E_SYNTAX"]);
        assert_eq!(error_codes("x = 1 2\n"), vec!["E_SYNTAX"]);
        assert_eq!(error_codes("variable x\n"), vec!["E_UNINITIALIZED"]);
        assert_eq!(error_codes("f() = 3\n"), vec!["E_INVALID_ASSIGNMENT"]);
    }

    #[test]
    fn statement_does_not_continue_on_next_line() {
        let module = parse_ok("x = a\n(b)\n");
        assert_eq!(module.stmts.len(), 2);
    }

    #[test]
    fn errors_recover_at_next_line() {
        let output = parse_source(FileId(0), "x = (1 +\ny = 2\nz = )\nw = 4\n");
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec!["E_SYNTAX", "E_SYNTAX"]);
        assert_eq!(output.module.stmts.len(), 1);
    }

    #[test]
    fn test_blocks() {
        let module =
            parse_ok("test \"adds numbers\"\n    assert(1 + 1 == 2)\n\ntest = 5\nprint(test)\n");
        assert_eq!(module.stmts.len(), 3);
        let StmtKind::Test { name, body, .. } = &module.stmts[0].kind else {
            panic!()
        };
        assert_eq!(name, "adds numbers");
        assert_eq!(body.stmts.len(), 1);
        assert!(matches!(module.stmts[1].kind, StmtKind::Assign { .. }));
    }

    #[test]
    fn unavailable_features_are_recorded() {
        let module = parse_ok("foreign \"c\"\n    function free(address: pointer)\nx = 1\n");
        assert!(matches!(
            module.stmts[0].kind,
            StmtKind::Unavailable {
                milestone: "M3",
                ..
            }
        ));
        assert_eq!(module.stmts.len(), 2);
    }
}
