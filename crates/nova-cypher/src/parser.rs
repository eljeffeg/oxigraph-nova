//! Recursive-descent parser for the openCypher subset.
//!
//! Grammar implemented (informal EBNF; keywords are case-insensitive):
//!
//! ```text
//! program    := 'MATCH' pattern ('WHERE' expr)? ( readTail | writeTail )
//!             | writeTail                            -- pattern-less write
//! readTail   := 'RETURN' returnClause
//!               ('ORDER' 'BY' orderItem (',' orderItem)*)?
//!               ('SKIP' int)? ('LIMIT' int)?
//! writeTail  := writeClause+                          -- no RETURN allowed
//! writeClause:= createClause | setClause | deleteClause | removeClause
//! createClause := 'CREATE' pattern
//! setClause    := 'SET' setItem (',' setItem)*
//! setItem      := IDENT '.' IDENT '=' expr | IDENT ':' IDENT
//! deleteClause := 'DETACH'? 'DELETE' IDENT (',' IDENT)*
//! removeClause := 'REMOVE' removeItem (',' removeItem)*
//! removeItem   := IDENT '.' IDENT | IDENT ':' IDENT
//! pattern    := nodePattern (relPattern nodePattern)*
//! nodePattern:= '(' IDENT? (':' IDENT)* properties? ')'
//! relPattern := '-' relDetail? '-' '>'      -- Right
//!             | '<' '-' relDetail? '-'      -- Left
//!             | '-' relDetail? '-'          -- Either
//! relDetail  := '[' IDENT? (':' IDENT)? varLength? properties? ']'
//! varLength  := '*' INT? ('..' INT?)?
//! properties := '{' (IDENT ':' literal (',' IDENT ':' literal)*)? '}'
//! returnClause := 'DISTINCT'? returnItem (',' returnItem)*
//! returnItem   := expr ('AS' IDENT)?
//! ```
//!
//! Explicitly rejected (clear error, not a panic): `WITH`, `MERGE`,
//! `OPTIONAL MATCH`, `UNION`, a second `MATCH` clause, `RETURN` after any
//! write clause, and semicolon-separated multi-statement scripts.

use crate::ast::*;
use crate::lexer::{Spanned, Tok, lex};

pub struct Parser {
    toks: Vec<Spanned>,
    pos: usize,
}

/// Either of the two top-level forms a Cypher program can take, as
/// distinguished (only) by whether a `RETURN` or a write clause follows the
/// optional `MATCH`/`WHERE` prefix.
enum Program {
    Query(CypherQuery),
    Statement(CypherStatement),
}

fn parse_program(src: &str) -> Result<Program, String> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    let program = p.parse_program()?;
    p.expect_eof()?;
    Ok(program)
}

/// Parses a full Cypher read-only query string into a [`CypherQuery`] AST.
/// Returns a clear error if `src` is actually a write statement (use
/// [`parse_statement`] instead).
pub fn parse(src: &str) -> Result<CypherQuery, String> {
    match parse_program(src)? {
        Program::Query(q) => Ok(q),
        Program::Statement(_) => Err(
            "this looks like a write statement (CREATE/SET/DELETE/REMOVE) — use \
             `parse_statement`/`parse_and_lower_update` instead of the read-only \
             `parse`/`parse_and_lower`"
                .to_string(),
        ),
    }
}

/// Parses a full Cypher write statement string into a [`CypherStatement`]
/// AST. Returns a clear error if `src` is actually a read-only query (use
/// [`parse`] instead).
pub fn parse_statement(src: &str) -> Result<CypherStatement, String> {
    match parse_program(src)? {
        Program::Statement(s) => Ok(s),
        Program::Query(_) => Err("this looks like a read-only query (ends in RETURN) — use \
             `parse`/`parse_and_lower` instead of `parse_statement`/`parse_and_lower_update`"
            .to_string()),
    }
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_pos(&self) -> usize {
        self.toks[self.pos].pos
    }

    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn err(&self, msg: impl Into<String>) -> String {
        format!("{} (at byte {})", msg.into(), self.peek_pos())
    }

    fn expect_eof(&self) -> Result<(), String> {
        if *self.peek() == Tok::Eof {
            Ok(())
        } else {
            Err(self.err(format!("unexpected trailing input: {}", self.peek())))
        }
    }

    /// True if the current token is `Ident(s)` matching `kw` case-insensitively.
    fn at_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_keyword(&mut self, kw: &str) -> Result<(), String> {
        if self.at_keyword(kw) {
            self.advance();
            Ok(())
        } else {
            Err(self.err(format!("expected keyword `{kw}`, found {}", self.peek())))
        }
    }

    fn eat_tok(&mut self, expected: &Tok) -> Result<(), String> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            Err(self.err(format!("expected {expected}, found {}", self.peek())))
        }
    }

    fn eat_ident(&mut self) -> Result<String, String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.advance();
                Ok(s)
            }
            other => Err(self.err(format!("expected identifier, found {other}"))),
        }
    }

    // ── Top-level program ────────────────────────────────────────────────

    /// True if the current token is a keyword that starts a write clause.
    fn at_write_clause_start(&self) -> bool {
        self.at_keyword("CREATE")
            || self.at_keyword("SET")
            || self.at_keyword("DELETE")
            || self.at_keyword("DETACH")
            || self.at_keyword("REMOVE")
    }

    fn parse_program(&mut self) -> Result<Program, String> {
        let (pattern, r#where) = if self.at_keyword("MATCH") {
            self.advance();
            let pattern = self.parse_pattern()?;

            if self.at_keyword("MATCH") {
                return Err(self.err("a second MATCH clause is not supported (single MATCH only)"));
            }
            for kw in ["OPTIONAL", "MERGE", "WITH", "UNION", "UNWIND"] {
                if self.at_keyword(kw) {
                    return Err(self.err(format!("`{kw}` is not supported")));
                }
            }

            let r#where = if self.at_keyword("WHERE") {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            (Some(pattern), r#where)
        } else {
            (None, None)
        };

        if self.at_write_clause_start() {
            let writes = self.parse_write_clauses()?;
            if self.at_keyword("RETURN") {
                return Err(self.err(
                    "`RETURN` after a write clause is not supported (write statements produce no result rows)",
                ));
            }
            return Ok(Program::Statement(CypherStatement {
                pattern,
                r#where,
                writes,
            }));
        }

        let Some(pattern) = pattern else {
            return Err(self.err(
                "expected `MATCH` (read query) or a write clause (CREATE/SET/DELETE/REMOVE)",
            ));
        };
        if self.at_keyword("MERGE") {
            return Err(self.err("`MERGE` is not supported"));
        }

        self.eat_keyword("RETURN")?;
        let r#return = self.parse_return_clause()?;

        let order_by = if self.at_keyword("ORDER") {
            self.advance();
            self.eat_keyword("BY")?;
            let mut items = vec![self.parse_order_item()?];
            while self.peek() == &Tok::Comma {
                self.advance();
                items.push(self.parse_order_item()?);
            }
            items
        } else {
            Vec::new()
        };

        let skip = if self.at_keyword("SKIP") {
            self.advance();
            Some(self.parse_uint()?)
        } else {
            None
        };

        let limit = if self.at_keyword("LIMIT") {
            self.advance();
            Some(self.parse_uint()?)
        } else {
            None
        };

        Ok(Program::Query(CypherQuery {
            pattern,
            r#where,
            r#return,
            order_by,
            skip,
            limit,
        }))
    }

    // ── Write clauses ──────────────────────────────────────────

    fn parse_write_clauses(&mut self) -> Result<Vec<WriteClause>, String> {
        let mut writes = self.parse_write_clause()?;
        while self.at_write_clause_start() {
            writes.extend(self.parse_write_clause()?);
        }
        Ok(writes)
    }

    /// Parses one write clause keyword's worth of input. Returns a `Vec`
    /// because `SET`/`REMOVE` accept comma-separated items that each become
    /// their own [`WriteClause`] (they have no shared multi-item variant),
    /// while `CREATE`/`DELETE`/`DETACH DELETE` always return a single-element
    /// `Vec`.
    fn parse_write_clause(&mut self) -> Result<Vec<WriteClause>, String> {
        if self.at_keyword("CREATE") {
            self.advance();
            Ok(vec![WriteClause::Create(self.parse_pattern()?)])
        } else if self.at_keyword("SET") {
            self.advance();
            self.parse_set_items()
        } else if self.at_keyword("DETACH") || self.at_keyword("DELETE") {
            Ok(vec![self.parse_delete_clause()?])
        } else if self.at_keyword("REMOVE") {
            self.advance();
            self.parse_remove_items()
        } else {
            Err(self.err(format!("expected a write clause, found {}", self.peek())))
        }
    }

    fn parse_set_items(&mut self) -> Result<Vec<WriteClause>, String> {
        let mut items = vec![self.parse_one_set_item()?];
        while self.peek() == &Tok::Comma {
            self.advance();
            items.push(self.parse_one_set_item()?);
        }
        Ok(items)
    }

    fn parse_one_set_item(&mut self) -> Result<WriteClause, String> {
        let variable = self.eat_ident()?;
        if self.peek() == &Tok::Dot {
            self.advance();
            let property = self.eat_ident()?;
            self.eat_tok(&Tok::Eq)?;
            let value = self.parse_expr()?;
            Ok(WriteClause::SetProperty {
                variable,
                property,
                value,
            })
        } else if self.peek() == &Tok::Colon {
            self.advance();
            let label = self.eat_ident()?;
            Ok(WriteClause::SetLabel { variable, label })
        } else {
            Err(self.err(format!(
                "expected `.` (property assignment) or `:` (label assignment) after `{variable}` in SET, found {}",
                self.peek()
            )))
        }
    }

    fn parse_delete_clause(&mut self) -> Result<WriteClause, String> {
        let detach = if self.at_keyword("DETACH") {
            self.advance();
            true
        } else {
            false
        };
        self.eat_keyword("DELETE")?;
        let mut variables = vec![self.eat_ident()?];
        while self.peek() == &Tok::Comma {
            self.advance();
            variables.push(self.eat_ident()?);
        }
        Ok(WriteClause::Delete { variables, detach })
    }

    fn parse_remove_items(&mut self) -> Result<Vec<WriteClause>, String> {
        let mut items = vec![self.parse_one_remove_item()?];
        while self.peek() == &Tok::Comma {
            self.advance();
            items.push(self.parse_one_remove_item()?);
        }
        Ok(items)
    }

    fn parse_one_remove_item(&mut self) -> Result<WriteClause, String> {
        let variable = self.eat_ident()?;
        if self.peek() == &Tok::Dot {
            self.advance();
            let property = self.eat_ident()?;
            Ok(WriteClause::RemoveProperty { variable, property })
        } else if self.peek() == &Tok::Colon {
            self.advance();
            let label = self.eat_ident()?;
            Ok(WriteClause::RemoveLabel { variable, label })
        } else {
            Err(self.err(format!(
                "expected `.` (property removal) or `:` (label removal) after `{variable}` in REMOVE, found {}",
                self.peek()
            )))
        }
    }

    fn parse_uint(&mut self) -> Result<u64, String> {
        match self.advance() {
            Tok::Int(n) if n >= 0 => Ok(n as u64),
            other => Err(self.err(format!("expected a non-negative integer, found {other}"))),
        }
    }

    // ── Pattern ──────────────────────────────────────────────────────────

    fn parse_pattern(&mut self) -> Result<Pattern, String> {
        let start = self.parse_node_pattern()?;
        let mut hops = Vec::new();
        while let Tok::Dash | Tok::Lt = self.peek() {
            let rel = self.parse_rel_pattern()?;
            let node = self.parse_node_pattern()?;
            hops.push((rel, node));
        }
        Ok(Pattern { start, hops })
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern, String> {
        self.eat_tok(&Tok::LParen)?;
        let mut node = NodePattern::default();

        if let Tok::Ident(_) = self.peek() {
            node.variable = Some(self.eat_ident()?);
        }
        while self.peek() == &Tok::Colon {
            self.advance();
            node.labels.push(self.eat_ident()?);
        }
        if self.peek() == &Tok::LBrace {
            node.properties = self.parse_properties()?;
        }

        self.eat_tok(&Tok::RParen)?;
        Ok(node)
    }

    fn parse_rel_pattern(&mut self) -> Result<RelPattern, String> {
        let left_arrow = if self.peek() == &Tok::Lt {
            self.advance();
            true
        } else {
            false
        };
        self.eat_tok(&Tok::Dash)?;

        let mut rel = RelPattern::default();
        if self.peek() == &Tok::LBracket {
            self.advance();
            if let Tok::Ident(_) = self.peek() {
                rel.variable = Some(self.eat_ident()?);
            }
            if self.peek() == &Tok::Colon {
                self.advance();
                rel.rel_type = Some(self.eat_ident()?);
            }
            if self.peek() == &Tok::Star {
                self.advance();
                rel.var_length = Some(self.parse_var_length()?);
            }
            if self.peek() == &Tok::LBrace {
                rel.properties = self.parse_properties()?;
            }

            self.eat_tok(&Tok::RBracket)?;
        }

        self.eat_tok(&Tok::Dash)?;
        let right_arrow = if self.peek() == &Tok::Gt {
            self.advance();
            true
        } else {
            false
        };

        rel.direction = RelDirectionOrDefault(match (left_arrow, right_arrow) {
            (true, false) => RelDirection::Left,
            (false, true) => RelDirection::Right,
            (false, false) => RelDirection::Either,
            (true, true) => {
                return Err(self.err("a relationship cannot point both `<-` and `->`"));
            }
        });

        Ok(rel)
    }

    fn parse_var_length(&mut self) -> Result<VarLength, String> {
        // '*' already consumed. Grammar from here: INT? ('..' INT?)?
        let min = if let Tok::Int(_) = self.peek() {
            match self.advance() {
                Tok::Int(n) if n >= 0 => Some(n as u64),
                _ => unreachable!(),
            }
        } else {
            None
        };

        if self.peek() == &Tok::DotDot {
            self.advance();
            let max = if let Tok::Int(_) = self.peek() {
                match self.advance() {
                    Tok::Int(n) if n >= 0 => Some(n as u64),
                    _ => unreachable!(),
                }
            } else {
                None
            };
            Ok(VarLength { min, max })
        } else {
            // `*n` with no `..` means an exact length of n (min == max).
            Ok(VarLength { min, max: min })
        }
    }

    fn parse_properties(&mut self) -> Result<Vec<(String, Literal)>, String> {
        self.eat_tok(&Tok::LBrace)?;
        let mut props = Vec::new();
        if self.peek() != &Tok::RBrace {
            loop {
                let key = self.eat_ident()?;
                self.eat_tok(&Tok::Colon)?;
                let value = self.parse_literal()?;
                props.push((key, value));
                if self.peek() == &Tok::Comma {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.eat_tok(&Tok::RBrace)?;
        Ok(props)
    }

    fn parse_literal(&mut self) -> Result<Literal, String> {
        match self.advance() {
            Tok::Str(s) => Ok(Literal::Str(s)),
            Tok::Int(n) => Ok(Literal::Int(n)),
            Tok::Float(n) => Ok(Literal::Float(n)),
            Tok::Dash => match self.advance() {
                Tok::Int(n) => Ok(Literal::Int(-n)),
                Tok::Float(n) => Ok(Literal::Float(-n)),
                other => Err(self.err(format!("expected number after `-`, found {other}"))),
            },
            Tok::Ident(s) if s.eq_ignore_ascii_case("true") => Ok(Literal::Bool(true)),
            Tok::Ident(s) if s.eq_ignore_ascii_case("false") => Ok(Literal::Bool(false)),
            Tok::Ident(s) if s.eq_ignore_ascii_case("null") => Ok(Literal::Null),
            other => Err(self.err(format!("expected a literal value, found {other}"))),
        }
    }

    // ── RETURN / ORDER BY ────────────────────────────────────────────────

    fn parse_return_clause(&mut self) -> Result<ReturnClause, String> {
        let distinct = if self.at_keyword("DISTINCT") {
            self.advance();
            true
        } else {
            false
        };

        let mut items = vec![self.parse_return_item()?];
        while self.peek() == &Tok::Comma {
            self.advance();
            items.push(self.parse_return_item()?);
        }
        Ok(ReturnClause { distinct, items })
    }

    fn parse_return_item(&mut self) -> Result<ReturnItem, String> {
        let expr = self.parse_expr()?;
        let alias = if self.at_keyword("AS") {
            self.advance();
            Some(self.eat_ident()?)
        } else {
            None
        };
        Ok(ReturnItem { expr, alias })
    }

    fn parse_order_item(&mut self) -> Result<OrderItem, String> {
        let expr = self.parse_expr()?;
        let descending = if self.at_keyword("DESC") {
            self.advance();
            true
        } else if self.at_keyword("ASC") {
            self.advance();
            false
        } else {
            false
        };
        Ok(OrderItem { expr, descending })
    }

    // ── Expressions (recursive-descent precedence climbing) ─────────────

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_and()?;
        while self.at_keyword("OR") {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_not()?;
        while self.at_keyword("AND") {
            self.advance();
            let rhs = self.parse_not()?;
            lhs = Expr::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr, String> {
        if self.at_keyword("NOT") {
            self.advance();
            let inner = self.parse_not()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_additive()?;
        let op = match self.peek() {
            Tok::Eq => Some(Expr::Eq as fn(_, _) -> Expr),
            Tok::Ne => Some(Expr::Ne as fn(_, _) -> Expr),
            Tok::Lt => Some(Expr::Lt as fn(_, _) -> Expr),
            Tok::Le => Some(Expr::Le as fn(_, _) -> Expr),
            Tok::Gt => Some(Expr::Gt as fn(_, _) -> Expr),
            Tok::Ge => Some(Expr::Ge as fn(_, _) -> Expr),
            _ => None,
        };
        if let Some(ctor) = op {
            self.advance();
            let rhs = self.parse_additive()?;
            Ok(ctor(Box::new(lhs), Box::new(rhs)))
        } else {
            Ok(lhs)
        }
    }

    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            match self.peek() {
                Tok::Plus => {
                    self.advance();
                    let rhs = self.parse_multiplicative()?;
                    lhs = Expr::Add(Box::new(lhs), Box::new(rhs));
                }
                Tok::Dash => {
                    self.advance();
                    let rhs = self.parse_multiplicative()?;
                    lhs = Expr::Sub(Box::new(lhs), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_unary()?;
        loop {
            match self.peek() {
                Tok::Star => {
                    self.advance();
                    let rhs = self.parse_unary()?;
                    lhs = Expr::Mul(Box::new(lhs), Box::new(rhs));
                }
                Tok::Slash => {
                    self.advance();
                    let rhs = self.parse_unary()?;
                    lhs = Expr::Div(Box::new(lhs), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.peek() == &Tok::Dash {
            self.advance();
            let inner = self.parse_unary()?;
            Ok(Expr::Neg(Box::new(inner)))
        } else {
            self.parse_atom()
        }
    }

    fn parse_atom(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Tok::LParen => {
                self.advance();
                let inner = self.parse_expr()?;
                self.eat_tok(&Tok::RParen)?;
                Ok(inner)
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Literal::Str(s)))
            }
            Tok::Int(n) => {
                self.advance();
                Ok(Expr::Literal(Literal::Int(n)))
            }
            Tok::Float(n) => {
                self.advance();
                Ok(Expr::Literal(Literal::Float(n)))
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("true") => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(true)))
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("false") => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(false)))
            }
            Tok::Ident(s) if s.eq_ignore_ascii_case("null") => {
                self.advance();
                Ok(Expr::Literal(Literal::Null))
            }
            Tok::Ident(name) => {
                self.advance();
                let mut expr = Expr::Variable(name);
                while self.peek() == &Tok::Dot {
                    self.advance();
                    let prop = self.eat_ident()?;
                    expr = Expr::Property(Box::new(expr), prop);
                }
                Ok(expr)
            }
            other => Err(self.err(format!("expected an expression, found {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_match_return() {
        let q = parse("MATCH (n:Person) RETURN n").unwrap();
        assert_eq!(q.pattern.start.variable.as_deref(), Some("n"));
        assert_eq!(q.pattern.start.labels, vec!["Person".to_string()]);
        assert!(q.pattern.hops.is_empty());
        assert_eq!(q.r#return.items.len(), 1);
        assert_eq!(q.r#return.items[0].expr, Expr::Variable("n".into()));
    }

    #[test]
    fn parses_relationship_pattern_with_direction() {
        let q = parse("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a, b").unwrap();
        assert_eq!(q.pattern.hops.len(), 1);
        let (rel, node) = &q.pattern.hops[0];
        assert_eq!(rel.rel_type.as_deref(), Some("KNOWS"));
        assert_eq!(rel.direction.0, RelDirection::Right);
        assert_eq!(node.variable.as_deref(), Some("b"));
    }

    #[test]
    fn parses_left_direction() {
        let q = parse("MATCH (a)<-[:KNOWS]-(b) RETURN a").unwrap();
        assert_eq!(q.pattern.hops[0].0.direction.0, RelDirection::Left);
    }

    #[test]
    fn parses_where_clause() {
        let q = parse("MATCH (n:Person) WHERE n.age > 30 RETURN n.name").unwrap();
        assert!(q.r#where.is_some());
        match q.r#where.unwrap() {
            Expr::Gt(l, r) => {
                assert_eq!(
                    *l,
                    Expr::Property(Box::new(Expr::Variable("n".into())), "age".into())
                );
                assert_eq!(*r, Expr::Literal(Literal::Int(30)));
            }
            other => panic!("expected Gt, got {other:?}"),
        }
    }

    #[test]
    fn parses_order_by_skip_limit_distinct() {
        let q = parse(
            "MATCH (n:Person) RETURN DISTINCT n.name AS name ORDER BY n.name DESC SKIP 5 LIMIT 10",
        )
        .unwrap();
        assert!(q.r#return.distinct);
        assert_eq!(q.r#return.items[0].alias.as_deref(), Some("name"));
        assert_eq!(q.order_by.len(), 1);
        assert!(q.order_by[0].descending);
        assert_eq!(q.skip, Some(5));
        assert_eq!(q.limit, Some(10));
    }

    #[test]
    fn parses_variable_length_relationship() {
        let q = parse("MATCH (a)-[:KNOWS*1..3]->(b) RETURN a").unwrap();
        let vl = q.pattern.hops[0].0.var_length.unwrap();
        assert_eq!(vl.min, Some(1));
        assert_eq!(vl.max, Some(3));
    }

    #[test]
    fn rejects_second_match_clause() {
        assert!(parse("MATCH (a) MATCH (b) RETURN a").is_err());
    }

    #[test]
    fn rejects_create_clause() {
        assert!(parse("MATCH (a) CREATE (b) RETURN a").is_err());
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse("MATCH (a) RETURN a EXTRA").is_err());
    }
}
