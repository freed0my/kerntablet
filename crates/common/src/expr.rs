//! A tiny arithmetic expression language for plugin manifests: turns the
//! user's settings into the numbers a plugin's BPF program reads. Everything
//! is `f64`; booleans are 0 / 1.
//!
//!   numbers      1, 0.5, 1e9
//!   variables    settings, `lets`, tablet constants, `d` in tables
//!   operators    || && == != < <= > >= + - * / % ^ (power, right-assoc), unary - !
//!   functions    pow exp ln log2 log10 sqrt abs floor ceil round trunc
//!                sin cos tan sinh cosh tanh atan2 min max clamp
//!                if(cond, a, b) isnan isfinite q20(x) = x * 2^20

use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Var(String),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

const FUNCS: &[(&str, usize)] = &[
    ("pow", 2),
    ("exp", 1),
    ("ln", 1),
    ("log2", 1),
    ("log10", 1),
    ("sqrt", 1),
    ("abs", 1),
    ("floor", 1),
    ("ceil", 1),
    ("round", 1),
    ("trunc", 1),
    ("sin", 1),
    ("cos", 1),
    ("tan", 1),
    ("sinh", 1),
    ("cosh", 1),
    ("tanh", 1),
    ("atan2", 2),
    ("min", 2),
    ("max", 2),
    ("clamp", 3),
    ("if", 3),
    ("isnan", 1),
    ("isfinite", 1),
    ("q20", 1),
];

pub fn parse(src: &str) -> Result<Expr, Error> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0 };
    let e = p.or()?;
    if p.pos != p.toks.len() {
        return Err(Error(format!("unexpected '{}' in \"{src}\"", p.toks[p.pos])));
    }
    Ok(e)
}

/// Parses `name = expr` (used for `lets`).
pub fn parse_let(src: &str) -> Result<(String, Expr), Error> {
    let (name, rhs) = src.split_once('=').ok_or_else(|| Error(format!("expected `name = expr`: \"{src}\"")))?;
    let name = name.trim();
    if !is_ident(name) {
        return Err(Error(format!("bad variable name \"{name}\"")));
    }
    Ok((name.to_string(), parse(rhs)?))
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Sym(&'static str),
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Num(n) => write!(f, "{n}"),
            Tok::Ident(s) => f.write_str(s),
            Tok::Sym(s) => f.write_str(s),
        }
    }
}

fn lex(src: &str) -> Result<Vec<Tok>, Error> {
    const SYMS: &[&str] = &["||", "&&", "==", "!=", "<=", ">=", "<", ">", "+", "-", "*", "/", "%", "^", "!", "(", ")", ","];
    let b = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    'outer: while i < b.len() {
        let c = b[i] as char;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let mut j = i + 1;
                if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                if j < b.len() && b[j].is_ascii_digit() {
                    i = j;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let s = &src[start..i];
            out.push(Tok::Num(s.parse().map_err(|_| Error(format!("bad number \"{s}\"")))?));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(src[start..i].to_string()));
            continue;
        }
        for s in SYMS {
            if src[i..].starts_with(s) {
                out.push(Tok::Sym(s));
                i += s.len();
                continue 'outer;
            }
        }
        return Err(Error(format!("unexpected character '{c}' in \"{src}\"")));
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek_sym(&self) -> Option<&'static str> {
        match self.toks.get(self.pos) {
            Some(Tok::Sym(s)) => Some(s),
            _ => None,
        }
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.peek_sym() == Some(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, s: &str) -> Result<(), Error> {
        if self.eat(s) {
            Ok(())
        } else {
            Err(Error(format!(
                "expected '{s}', found {}",
                self.toks.get(self.pos).map_or("end of expression".to_string(), |t| format!("'{t}'"))
            )))
        }
    }

    fn binary(&mut self, ops: &[(&str, Op)], next: fn(&mut Self) -> Result<Expr, Error>) -> Result<Expr, Error> {
        let mut lhs = next(self)?;
        'again: loop {
            for (s, op) in ops {
                if self.eat(s) {
                    let rhs = next(self)?;
                    lhs = Expr::Bin(*op, Box::new(lhs), Box::new(rhs));
                    continue 'again;
                }
            }
            return Ok(lhs);
        }
    }

    fn or(&mut self) -> Result<Expr, Error> {
        self.binary(&[("||", Op::Or)], Self::and)
    }

    fn and(&mut self) -> Result<Expr, Error> {
        self.binary(&[("&&", Op::And)], Self::cmp)
    }

    fn cmp(&mut self) -> Result<Expr, Error> {
        self.binary(
            &[("==", Op::Eq), ("!=", Op::Ne), ("<=", Op::Le), (">=", Op::Ge), ("<", Op::Lt), (">", Op::Gt)],
            Self::add,
        )
    }

    fn add(&mut self) -> Result<Expr, Error> {
        self.binary(&[("+", Op::Add), ("-", Op::Sub)], Self::mul)
    }

    fn mul(&mut self) -> Result<Expr, Error> {
        self.binary(&[("*", Op::Mul), ("/", Op::Div), ("%", Op::Rem)], Self::unary)
    }

    fn unary(&mut self) -> Result<Expr, Error> {
        if self.eat("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        if self.eat("!") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat("+") {
            return self.unary();
        }
        self.power()
    }

    /// `a ^ b ^ c` = `a ^ (b ^ c)`; binds tighter than unary minus on the left.
    fn power(&mut self) -> Result<Expr, Error> {
        let base = self.atom()?;
        if self.eat("^") {
            let exp = self.unary()?;
            return Ok(Expr::Bin(Op::Pow, Box::new(base), Box::new(exp)));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Expr, Error> {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(Expr::Num(n))
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                if !self.eat("(") {
                    return Ok(Expr::Var(name));
                }
                let mut args = Vec::new();
                if !self.eat(")") {
                    loop {
                        args.push(self.or()?);
                        if self.eat(")") {
                            break;
                        }
                        self.expect(",")?;
                    }
                }
                let Some((_, arity)) = FUNCS.iter().find(|(f, _)| *f == name) else {
                    return Err(Error(format!("unknown function {name}()")));
                };
                if args.len() != *arity {
                    return Err(Error(format!("{name}() takes {arity} argument(s), got {}", args.len())));
                }
                Ok(Expr::Call(name, args))
            }
            Some(Tok::Sym("(")) => {
                self.pos += 1;
                let e = self.or()?;
                self.expect(")")?;
                Ok(e)
            }
            Some(t) => Err(Error(format!("unexpected '{t}'"))),
            None => Err(Error("unexpected end of expression".into())),
        }
    }
}

pub type Vars = HashMap<String, f64>;

fn b(v: bool) -> f64 {
    if v {
        1.0
    } else {
        0.0
    }
}

impl Expr {
    pub fn eval(&self, vars: &Vars) -> Result<f64, Error> {
        Ok(match self {
            Expr::Num(n) => *n,
            Expr::Var(v) => *vars.get(v).ok_or_else(|| Error(format!("unknown variable \"{v}\"")))?,
            Expr::Neg(e) => -e.eval(vars)?,
            Expr::Not(e) => b(e.eval(vars)? == 0.0),
            Expr::Bin(op, l, r) => {
                let (l, r) = (l.eval(vars)?, r.eval(vars)?);
                match op {
                    Op::Or => b(l != 0.0 || r != 0.0),
                    Op::And => b(l != 0.0 && r != 0.0),
                    Op::Eq => b(l == r),
                    Op::Ne => b(l != r),
                    Op::Lt => b(l < r),
                    Op::Le => b(l <= r),
                    Op::Gt => b(l > r),
                    Op::Ge => b(l >= r),
                    Op::Add => l + r,
                    Op::Sub => l - r,
                    Op::Mul => l * r,
                    Op::Div => l / r,
                    Op::Rem => l % r,
                    Op::Pow => l.powf(r),
                }
            }
            Expr::Call(f, args) => {
                let a: Vec<f64> = args.iter().map(|e| e.eval(vars)).collect::<Result<_, _>>()?;
                match (f.as_str(), a.as_slice()) {
                    ("pow", [x, y]) => x.powf(*y),
                    ("exp", [x]) => x.exp(),
                    ("ln", [x]) => x.ln(),
                    ("log2", [x]) => x.log2(),
                    ("log10", [x]) => x.log10(),
                    ("sqrt", [x]) => x.sqrt(),
                    ("abs", [x]) => x.abs(),
                    ("floor", [x]) => x.floor(),
                    ("ceil", [x]) => x.ceil(),
                    ("round", [x]) => x.round(),
                    ("trunc", [x]) => x.trunc(),
                    ("sin", [x]) => x.sin(),
                    ("cos", [x]) => x.cos(),
                    ("tan", [x]) => x.tan(),
                    ("sinh", [x]) => x.sinh(),
                    ("cosh", [x]) => x.cosh(),
                    ("tanh", [x]) => x.tanh(),
                    ("atan2", [y, x]) => y.atan2(*x),
                    ("min", [x, y]) => x.min(*y),
                    ("max", [x, y]) => x.max(*y),
                    // NaN stays NaN (like C#'s Math.Clamp), so isnan() can catch it.
                    ("clamp", [x, lo, hi]) => {
                        if x.is_nan() {
                            *x
                        } else {
                            x.max(*lo).min(*hi)
                        }
                    }
                    ("if", [c, t, e]) => {
                        if *c != 0.0 && !c.is_nan() {
                            *t
                        } else {
                            *e
                        }
                    }
                    ("isnan", [x]) => b(x.is_nan()),
                    ("isfinite", [x]) => b(x.is_finite()),
                    ("q20", [x]) => x * (1u64 << 20) as f64,
                    _ => return Err(Error(format!("bad call {f}()"))),
                }
            }
        })
    }

    /// Variable names this expression refers to.
    pub fn vars(&self, out: &mut Vec<String>) {
        match self {
            Expr::Num(_) => {}
            Expr::Var(v) => out.push(v.clone()),
            Expr::Neg(e) | Expr::Not(e) => e.vars(out),
            Expr::Bin(_, l, r) => {
                l.vars(out);
                r.vars(out);
            }
            Expr::Call(_, args) => args.iter().for_each(|a| a.vars(out)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str) -> f64 {
        let mut v = Vars::new();
        v.insert("x".into(), 2.0);
        parse(s).unwrap().eval(&v).unwrap()
    }

    #[test]
    fn precedence() {
        assert_eq!(ev("1 + 2 * 3"), 7.0);
        assert_eq!(ev("-2 ^ 2"), -4.0);
        assert_eq!(ev("2 ^ 3 ^ 2"), 512.0);
        assert_eq!(ev("2 ^ -1"), 0.5);
        assert_eq!(ev("(1 + 2) * 3"), 9.0);
        assert_eq!(ev("1 < 2 && 3 > 4 || x == 2"), 1.0);
        assert_eq!(ev("!0 + 1e3 - .5"), 1000.5);
        assert_eq!(ev("10 % 4 - x"), 0.0);
    }

    #[test]
    fn functions() {
        assert_eq!(ev("if(x > 1, 10, 20)"), 10.0);
        assert_eq!(ev("clamp(5, 0, 1)"), 1.0);
        assert_eq!(ev("isnan(clamp(0 / 0, 0, 1))"), 1.0);
        assert_eq!(ev("q20(0.5)"), 524288.0);
        assert_eq!(ev("pow(x, 10)"), 1024.0);
        assert_eq!(ev("1 / 0"), f64::INFINITY);
    }

    #[test]
    fn errors() {
        assert!(parse("1 +").is_err());
        assert!(parse("foo(1)").is_err());
        assert!(parse("pow(1)").is_err());
        assert!(parse("(1").is_err());
        assert!(parse("1 $ 2").is_err());
        assert!(parse("y").unwrap().eval(&Vars::new()).is_err());
        assert_eq!(parse_let("a = 1 + 1").unwrap().0, "a");
        assert!(parse_let("1a = 2").is_err());
    }
}
