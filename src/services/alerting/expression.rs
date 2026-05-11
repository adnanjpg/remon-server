//! Expression DSL — parser, AST, and evaluator for the comparison.
//!
//! v1 grammar (recursive-descent friendly):
//!
//! ```text
//! rule       := metric_ref WS comparator WS number
//! metric_ref := ident "." ident label_set?
//! label_set  := "{" pair ( "," pair )* "}"
//! pair       := ident "=" '"' string '"'
//! ident      := [a-z][a-z0-9_]*
//! comparator := ">" | ">=" | "<" | "<=" | "==" | "!="
//! number     := [0-9]+ ( "." [0-9]+ )?
//! string     := JSON-style string (no \uXXXX escapes — keep it small)
//! ```
//!
//! Errors point at a byte offset in the source string with a one-line
//! message; the REST layer surfaces them as 400 BAD_REQUEST so operators
//! see exactly what's wrong on the rule editor.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparator {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

impl Comparator {
    pub fn as_str(&self) -> &'static str {
        match self {
            Comparator::Gt => ">",
            Comparator::Ge => ">=",
            Comparator::Lt => "<",
            Comparator::Le => "<=",
            Comparator::Eq => "==",
            Comparator::Ne => "!=",
        }
    }

    /// Apply to a sample. Float equality uses `f64::EPSILON` so a probe
    /// emitting `42.0` vs `42.0000000001` doesn't flap on `==`.
    pub fn evaluate(self, value: f64, threshold: f64) -> bool {
        match self {
            Comparator::Gt => value > threshold,
            Comparator::Ge => value >= threshold,
            Comparator::Lt => value < threshold,
            Comparator::Le => value <= threshold,
            Comparator::Eq => (value - threshold).abs() < f64::EPSILON,
            Comparator::Ne => (value - threshold).abs() >= f64::EPSILON,
        }
    }
}

/// `cpu.usage_percent{mount_point="/"}` shape. `labels` is a sorted map
/// so two equivalent expressions (different label order) compare equal
/// and produce the same canonical SQL key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricRef {
    pub namespace: String,
    pub field: String,
    pub labels: BTreeMap<String, String>,
}

impl fmt::Display for MetricRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.namespace, self.field)?;
        if !self.labels.is_empty() {
            write!(f, "{{")?;
            for (i, (k, v)) in self.labels.iter().enumerate() {
                if i > 0 {
                    write!(f, ",")?;
                }
                write!(f, "{}=\"{}\"", k, v)?;
            }
            write!(f, "}}")?;
        }
        Ok(())
    }
}

/// One parsed rule: `metric_ref comparator threshold`.
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    pub metric: MetricRef,
    pub comparator: Comparator,
    pub threshold: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "alert expression parse error at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl std::error::Error for ParseError {}

/// Top-level entry: parse a complete rule expression. Trailing
/// whitespace is fine; trailing tokens are an error.
pub fn parse(input: &str) -> Result<Expression, ParseError> {
    let mut p = Parser::new(input);
    p.skip_ws();
    let metric = p.parse_metric_ref()?;
    p.skip_ws();
    let comparator = p.parse_comparator()?;
    p.skip_ws();
    let threshold = p.parse_number()?;
    p.skip_ws();
    if !p.eof() {
        return Err(p.err("unexpected trailing tokens"));
    }
    Ok(Expression {
        metric,
        comparator,
        threshold,
    })
}

// ===== Parser =====

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        ParseError {
            offset: self.pos,
            message: msg.into(),
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.bump();
            } else {
                break;
            }
        }
    }

    /// Match a literal string. Returns true and advances on match.
    fn eat(&mut self, s: &str) -> bool {
        if self.src[self.pos..].starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn parse_ident(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        let first = match self.peek() {
            Some(c) if c.is_ascii_lowercase() => c,
            Some(c) => {
                return Err(self.err(format!(
                    "expected identifier (lowercase letter), got '{}'",
                    c
                )));
            }
            None => return Err(self.err("expected identifier, got end of input")),
        };
        self.bump();
        let _ = first;
        while let Some(c) = self.peek() {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                self.bump();
            } else {
                break;
            }
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn parse_metric_ref(&mut self) -> Result<MetricRef, ParseError> {
        let namespace = self.parse_ident()?;
        if !self.eat(".") {
            return Err(self.err("expected '.' after namespace"));
        }
        let field = self.parse_ident()?;
        let labels = if self.peek() == Some('{') {
            self.parse_label_set()?
        } else {
            BTreeMap::new()
        };
        Ok(MetricRef {
            namespace,
            field,
            labels,
        })
    }

    fn parse_label_set(&mut self) -> Result<BTreeMap<String, String>, ParseError> {
        // Caller has confirmed peek == '{'.
        self.bump();
        self.skip_ws();
        let mut out = BTreeMap::new();
        // Empty {} accepted for symmetry.
        if self.peek() == Some('}') {
            self.bump();
            return Ok(out);
        }
        loop {
            self.skip_ws();
            let key = self.parse_ident()?;
            self.skip_ws();
            if !self.eat("=") {
                return Err(self.err("expected '=' after label key"));
            }
            self.skip_ws();
            let value = self.parse_string()?;
            if out.insert(key.clone(), value).is_some() {
                return Err(self.err(format!("duplicate label key '{}'", key)));
            }
            self.skip_ws();
            if self.eat(",") {
                continue;
            }
            if self.eat("}") {
                break;
            }
            return Err(self.err("expected ',' or '}' in label set"));
        }
        Ok(out)
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        if !self.eat("\"") {
            return Err(self.err("expected '\"' to open string literal"));
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string literal")),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => {
                        return Err(self.err(format!("unknown escape '\\{}'", other)));
                    }
                    None => return Err(self.err("dangling backslash in string")),
                },
                Some(c) => out.push(c),
            }
        }
    }

    fn parse_comparator(&mut self) -> Result<Comparator, ParseError> {
        // Two-char first to avoid eating the first char of `>=` / `<=` /
        // `==` / `!=` as a single-char operator.
        if self.eat(">=") {
            return Ok(Comparator::Ge);
        }
        if self.eat("<=") {
            return Ok(Comparator::Le);
        }
        if self.eat("==") {
            return Ok(Comparator::Eq);
        }
        if self.eat("!=") {
            return Ok(Comparator::Ne);
        }
        if self.eat(">") {
            return Ok(Comparator::Gt);
        }
        if self.eat("<") {
            return Ok(Comparator::Lt);
        }
        Err(self.err("expected comparator (>, >=, <, <=, ==, !=)"))
    }

    fn parse_number(&mut self) -> Result<f64, ParseError> {
        let start = self.pos;
        // Optional leading '-' for thresholds like `< -1.5`.
        if self.peek() == Some('-') {
            self.bump();
        }
        let mut has_int = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.bump();
                has_int = true;
            } else {
                break;
            }
        }
        if !has_int {
            return Err(self.err("expected number"));
        }
        if self.peek() == Some('.') {
            self.bump();
            let mut has_frac = false;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.bump();
                    has_frac = true;
                } else {
                    break;
                }
            }
            if !has_frac {
                return Err(self.err("expected fractional digits after '.'"));
            }
        }
        let text = &self.src[start..self.pos];
        text.parse::<f64>()
            .map_err(|_| self.err(format!("could not parse '{}' as number", text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn must_parse(s: &str) -> Expression {
        parse(s).unwrap_or_else(|e| panic!("expected '{}' to parse, got {}", s, e))
    }

    #[test]
    fn simple_rule() {
        let e = must_parse("cpu.usage_percent > 80");
        assert_eq!(e.metric.namespace, "cpu");
        assert_eq!(e.metric.field, "usage_percent");
        assert!(e.metric.labels.is_empty());
        assert_eq!(e.comparator, Comparator::Gt);
        assert_eq!(e.threshold, 80.0);
    }

    #[test]
    fn all_comparators() {
        assert_eq!(must_parse("a.b > 1").comparator, Comparator::Gt);
        assert_eq!(must_parse("a.b >= 1").comparator, Comparator::Ge);
        assert_eq!(must_parse("a.b < 1").comparator, Comparator::Lt);
        assert_eq!(must_parse("a.b <= 1").comparator, Comparator::Le);
        assert_eq!(must_parse("a.b == 1").comparator, Comparator::Eq);
        assert_eq!(must_parse("a.b != 1").comparator, Comparator::Ne);
    }

    #[test]
    fn label_set_single() {
        let e = must_parse("disk.used_bytes{mount_point=\"/\"} > 1073741824");
        assert_eq!(e.metric.namespace, "disk");
        assert_eq!(e.metric.field, "used_bytes");
        assert_eq!(e.metric.labels.get("mount_point"), Some(&"/".to_string()));
        assert_eq!(e.threshold, 1073741824.0);
    }

    #[test]
    fn label_set_multi_sorted() {
        let e = must_parse("probe.banned{probe_name=\"fail2ban\",jail=\"sshd\"} > 100");
        // BTreeMap: jail < probe_name alphabetically.
        let keys: Vec<&str> = e.metric.labels.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["jail", "probe_name"]);
    }

    #[test]
    fn label_value_with_escape() {
        let e = must_parse(r#"a.b{x="he said \"hi\""} > 0"#);
        assert_eq!(
            e.metric.labels.get("x"),
            Some(&"he said \"hi\"".to_string())
        );
    }

    #[test]
    fn negative_threshold() {
        let e = must_parse("a.b < -1.5");
        assert_eq!(e.threshold, -1.5);
    }

    #[test]
    fn float_threshold() {
        let e = must_parse("pressure.some_avg10{resource=\"cpu\"} >= 5.25");
        assert_eq!(e.threshold, 5.25);
    }

    #[test]
    fn whitespace_tolerant() {
        // Operators don't have to write tight expressions.
        let e = must_parse("  cpu.usage_percent   >    80   ");
        assert_eq!(e.metric.namespace, "cpu");
        assert_eq!(e.threshold, 80.0);
    }

    #[test]
    fn empty_label_set_ok() {
        let e = must_parse("cpu.usage_percent{} > 80");
        assert!(e.metric.labels.is_empty());
    }

    // ----- error cases -----

    #[test]
    fn missing_dot() {
        let err = parse("cpu_usage_percent > 80").unwrap_err();
        assert!(err.message.contains("expected '.'"));
    }

    #[test]
    fn missing_namespace() {
        let err = parse(".usage_percent > 80").unwrap_err();
        assert!(err.message.contains("identifier"));
    }

    #[test]
    fn unknown_comparator() {
        let err = parse("cpu.usage_percent ~ 80").unwrap_err();
        assert!(err.message.contains("comparator"));
    }

    #[test]
    fn missing_threshold() {
        let err = parse("cpu.usage_percent >").unwrap_err();
        assert!(err.message.contains("number"));
    }

    #[test]
    fn trailing_garbage_rejected() {
        let err = parse("cpu.usage_percent > 80 garbage").unwrap_err();
        assert!(err.message.contains("trailing"));
    }

    #[test]
    fn duplicate_label_key_rejected() {
        let err = parse("a.b{x=\"1\",x=\"2\"} > 0").unwrap_err();
        assert!(err.message.contains("duplicate"));
    }

    #[test]
    fn unterminated_string() {
        let err = parse(r#"a.b{x="abc} > 0"#).unwrap_err();
        assert!(err.message.contains("string"));
    }

    #[test]
    fn unknown_escape() {
        let err = parse(r#"a.b{x="\q"} > 0"#).unwrap_err();
        assert!(err.message.contains("escape"));
    }

    #[test]
    fn uppercase_ident_rejected() {
        let err = parse("CPU.usage > 80").unwrap_err();
        assert!(err.message.contains("identifier"));
    }

    // ----- Display round-trip (canonicalisation) -----

    #[test]
    fn metric_ref_display_is_canonical() {
        let e = must_parse("probe.banned{probe_name=\"fail2ban\",jail=\"sshd\"} > 100");
        // BTreeMap sorts keys alphabetically; output reflects that.
        assert_eq!(
            format!("{}", e.metric),
            "probe.banned{jail=\"sshd\",probe_name=\"fail2ban\"}"
        );
    }

    // ----- Comparator semantics -----

    #[test]
    fn comparator_evaluation() {
        assert!(Comparator::Gt.evaluate(81.0, 80.0));
        assert!(!Comparator::Gt.evaluate(80.0, 80.0));
        assert!(Comparator::Ge.evaluate(80.0, 80.0));
        assert!(Comparator::Eq.evaluate(80.0, 80.0));
        assert!(!Comparator::Eq.evaluate(80.0, 80.000_000_1));
        assert!(Comparator::Ne.evaluate(80.0, 81.0));
    }
}
