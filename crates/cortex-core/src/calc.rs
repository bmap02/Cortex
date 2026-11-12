//! Tiny expression evaluator for the calculator tool.
//! Supports + - * / % ^, parentheses, unary minus, constants pi/e and
//! functions sqrt abs ln log log2 sin cos tan floor ceil round exp.

pub fn eval(src: &str) -> Result<f64, String> {
    let mut p = Parser { s: src.as_bytes(), i: 0 };
    let v = p.expr()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("unexpected '{}' at position {}", p.s[p.i] as char, p.i));
    }
    Ok(v)
}

struct Parser<'a> { s: &'a [u8], i: usize }

impl Parser<'_> {
    fn ws(&mut self) { while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() { self.i += 1; } }
    fn peek(&mut self) -> Option<u8> { self.ws(); self.s.get(self.i).copied() }
    fn eat(&mut self, c: u8) -> bool { if self.peek() == Some(c) { self.i += 1; true } else { false } }

    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        loop {
            if self.eat(b'+') { v += self.term()?; }
            else if self.eat(b'-') { v -= self.term()?; }
            else { return Ok(v); }
        }
    }
    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.unary()?;
        loop {
            if self.eat(b'*') { v *= self.unary()?; }
            else if self.eat(b'/') {
                let d = self.unary()?;
                if d == 0.0 { return Err("division by zero".into()); }
                v /= d;
            }
            else if self.eat(b'%') {
                let d = self.unary()?;
                if d == 0.0 { return Err("modulo by zero".into()); }
                v %= d;
            }
            else { return Ok(v); }
        }
    }
    fn power(&mut self) -> Result<f64, String> {
        let base = self.atom()?;
        if self.eat(b'^') { Ok(base.powf(self.unary()?)) } else { Ok(base) } // right-assoc
    }
    fn unary(&mut self) -> Result<f64, String> {
        if self.eat(b'-') { return Ok(-self.unary()?); }
        if self.eat(b'+') { return self.unary(); }
        self.power()
    }
    fn atom(&mut self) -> Result<f64, String> {
        if self.eat(b'(') {
            let v = self.expr()?;
            if !self.eat(b')') { return Err("missing ')'".into()); }
            return Ok(v);
        }
        let start = self.i;
        match self.peek() {
            Some(c) if c.is_ascii_digit() || c == b'.' => {
                while self.i < self.s.len() && (self.s[self.i].is_ascii_digit() || self.s[self.i] == b'.' || self.s[self.i] == b'_') { self.i += 1; }
                if self.i < self.s.len() && (self.s[self.i] == b'e' || self.s[self.i] == b'E') {
                    let save = self.i;
                    self.i += 1;
                    if self.i < self.s.len() && (self.s[self.i] == b'+' || self.s[self.i] == b'-') { self.i += 1; }
                    if self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() { self.i += 1; }
                    } else { self.i = save; }
                }
                let t: String = std::str::from_utf8(&self.s[start..self.i]).unwrap().replace('_', "");
                t.parse().map_err(|_| format!("bad number '{t}'"))
            }
            Some(c) if c.is_ascii_alphabetic() => {
                while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric()) { self.i += 1; }
                let name = std::str::from_utf8(&self.s[start..self.i]).unwrap().to_lowercase();
                match name.as_str() {
                    "pi" => return Ok(std::f64::consts::PI),
                    "e" => return Ok(std::f64::consts::E),
                    _ => {}
                }
                if !self.eat(b'(') { return Err(format!("unknown name '{name}'")); }
                let x = self.expr()?;
                if !self.eat(b')') { return Err("missing ')'".into()); }
                Ok(match name.as_str() {
                    "sqrt" => x.sqrt(), "abs" => x.abs(), "ln" => x.ln(), "log" => x.log10(), "log2" => x.log2(),
                    "sin" => x.sin(), "cos" => x.cos(), "tan" => x.tan(), "exp" => x.exp(),
                    "floor" => x.floor(), "ceil" => x.ceil(), "round" => x.round(),
                    _ => return Err(format!("unknown function '{name}'")),
                })
            }
            Some(c) => Err(format!("unexpected '{}'", c as char)),
            None => Err("unexpected end of expression".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::eval;
    #[test]
    fn basics() {
        assert_eq!(eval("1 + 2 * 3").unwrap(), 7.0);
        assert_eq!(eval("(1 + 2) * 3").unwrap(), 9.0);
        assert_eq!(eval("2 ^ 3 ^ 2").unwrap(), 512.0);
        assert_eq!(eval("-2^2").unwrap(), -4.0);
        assert_eq!(eval("2^-1").unwrap(), 0.5);
        assert!((eval("sqrt(16) + log(1000)").unwrap() - 7.0).abs() < 1e-9);
        assert_eq!(eval("1_000 * 1.5e3").unwrap(), 1_500_000.0);
        assert!(eval("1/0").is_err());
        assert!(eval("5 % 0").is_err());
        assert_eq!(eval("7 % 3").unwrap(), 1.0);
    }
}
