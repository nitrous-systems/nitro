//! The calculator itself: a state machine and a formatter, and no UI.
//!
//! Nothing in this module knows that a widget exists. That is the point:
//! the arithmetic of a calculator is where its bugs live, and bugs in a
//! pure function are cheap to find — every rule below is checked by the
//! tests at the bottom of this file, which need no server, no window and
//! no toolkit.
//!
//! # The machine
//!
//! Three states, and the display tells you which one you are in:
//!
//! * **entry** — digits are being typed; the display *is* the string
//!   being typed, so a trailing `.` or a leading `0` shows as typed.
//! * **result** — a value is being shown ([`Engine::press`] of a digit
//!   starts a fresh entry and throws it away).
//! * **error** — division by zero, or a result that left the range of an
//!   `f64`. Only `C` (and the `⌫` that stands in for it) gets out.
//!
//! # Left to right, like a desk calculator
//!
//! `2 + 3 × 4 =` is **20**, not 14: each operator key completes the
//! operation already pending before starting the next one. That is what a
//! four-function desk calculator does, it is what the button layout
//! implies (there are no parentheses), and it is a deliberate choice
//! rather than a missing feature — precedence without parentheses is a
//! calculator that cannot express what it evaluates.

/// The four operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Addition.
    Add,
    /// Subtraction.
    Sub,
    /// Multiplication.
    Mul,
    /// Division.
    Div,
}

impl Op {
    /// The symbol shown on the button and in the history line.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Op::Add => "+",
            Op::Sub => "−",
            Op::Mul => "×",
            Op::Div => "÷",
        }
    }

    /// Apply the operation, or `None` for division by zero.
    #[must_use]
    pub fn apply(self, a: f64, b: f64) -> Option<f64> {
        let v = match self {
            Op::Add => a + b,
            Op::Sub => a - b,
            Op::Mul => a * b,
            Op::Div if b == 0.0 => return None,
            Op::Div => a / b,
        };
        v.is_finite().then_some(v)
    }
}

/// One press, whether it came from a button, the keyboard or `hey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// `0`–`9`.
    Digit(u8),
    /// The decimal point.
    Dot,
    /// An operator.
    Op(Op),
    /// `=`.
    Equals,
    /// `C`: back to zero, forgetting everything.
    Clear,
    /// `⌫`: drop the last character typed.
    Backspace,
    /// `±`: flip the sign.
    Negate,
}

impl Key {
    /// The key a typed character means, if any.
    ///
    /// Shared by the keyboard handler and by the buttons, so `7` on the
    /// keyboard and `7` on the screen cannot drift apart.
    #[must_use]
    pub fn from_text(text: &str) -> Option<Key> {
        let mut chars = text.chars();
        let c = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        Some(match c {
            '0'..='9' => Key::Digit(c as u8 - b'0'),
            '.' | ',' => Key::Dot,
            '+' => Key::Op(Op::Add),
            '-' | '−' => Key::Op(Op::Sub),
            '*' | 'x' | 'X' | '×' => Key::Op(Op::Mul),
            '/' | '÷' => Key::Op(Op::Div),
            '=' => Key::Equals,
            'c' | 'C' => Key::Clear,
            'n' | 'N' => Key::Negate,
            _ => return None,
        })
    }
}

/// How many significant digits the display carries.
///
/// Fifteen, because an `f64` holds between 15 and 17 significant decimal
/// digits and only the first 15 are *always* right: printing the 16th
/// turns `0.1 + 0.2` into `0.30000000000000004` on the screen of a
/// calculator whose user is entitled to `0.3`.
pub const DIGITS: usize = 15;

/// The largest base-10 exponent still shown in fixed notation.
///
/// One less than [`DIGITS`]: an exponent of 14 is a 15-digit integer,
/// which is exactly what `DIGITS` says can be printed without inventing
/// precision. Spelled out rather than computed from `DIGITS`, which is a
/// length and would need a cast to take part in this arithmetic.
const MAX_FIXED_EXP: i32 = 14;

const _: () = assert!(MAX_FIXED_EXP as usize == DIGITS - 1);

/// Hard cap on the length of the entry string.
///
/// [`DIGITS`] counts *significant* digits, so `0.000…` can be typed
/// indefinitely without ever reaching it. This is the backstop: enough
/// room for a sign, a point, fifteen digits and the leading zeros that
/// reach the smallest number the display shows in fixed notation.
const MAX_ENTRY: usize = 24;

/// The calculator's state machine.
#[derive(Debug)]
pub struct Engine {
    /// The number being typed, empty when a result is on show.
    entry: String,
    /// The left operand of the pending operation.
    acc: f64,
    /// The operator waiting for its right operand.
    pending: Option<Op>,
    /// True between an operator press and the first digit after it: the
    /// next operator *replaces* this one rather than operating.
    awaiting: bool,
    /// The value shown when nothing is being typed.
    result: f64,
    /// Set by division by zero or an overflow; only `C` clears it.
    error: bool,
    display: String,
    history: String,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// A calculator showing zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entry: String::new(),
            acc: 0.0,
            pending: None,
            awaiting: false,
            result: 0.0,
            error: false,
            display: "0".to_owned(),
            history: String::new(),
        }
    }

    /// What the display shows.
    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    /// The expression so far, for the history line: `7 + 8 =`, or empty.
    #[must_use]
    pub fn history(&self) -> &str {
        &self.history
    }

    /// Whether the last operation failed.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.error
    }

    /// The number the display stands for, ignoring an error.
    #[must_use]
    pub fn value(&self) -> f64 {
        if self.entry.is_empty() {
            self.result
        } else {
            self.entry.parse().unwrap_or(0.0)
        }
    }

    /// Apply one key.
    pub fn press(&mut self, key: Key) {
        // An error swallows everything but the two keys that clear it, so
        // a calculator showing `Error` cannot quietly go on computing
        // with a number nobody can see.
        if self.error && !matches!(key, Key::Clear | Key::Backspace) {
            return;
        }
        match key {
            Key::Digit(d) => self.digit(d),
            Key::Dot => self.dot(),
            Key::Op(op) => self.operator(op),
            Key::Equals => self.equals(),
            Key::Clear => *self = Engine::new(),
            Key::Backspace => self.backspace(),
            Key::Negate => self.negate(),
        }
        self.refresh();
    }

    fn digit(&mut self, d: u8) {
        self.awaiting = false;
        if self.entry == "0" {
            // A leading zero is a placeholder, not a digit.
            self.entry.clear();
        }
        if self.significant() >= DIGITS || self.entry.len() >= MAX_ENTRY {
            return;
        }
        self.entry.push((b'0' + d) as char);
    }

    fn dot(&mut self) {
        self.awaiting = false;
        if self.entry.is_empty() {
            self.entry.push('0');
        }
        if !self.entry.contains('.') {
            self.entry.push('.');
        }
    }

    fn negate(&mut self) {
        if self.entry.is_empty() {
            self.result = -self.result;
            return;
        }
        if self.entry == "0" {
            return;
        }
        if let Some(rest) = self.entry.strip_prefix('-') {
            self.entry = rest.to_owned();
        } else {
            self.entry.insert(0, '-');
        }
    }

    fn backspace(&mut self) {
        if self.error {
            *self = Engine::new();
            return;
        }
        if self.entry.is_empty() {
            // Nothing is being typed, so there is no character to drop;
            // the honest thing is to clear the value being shown.
            self.result = 0.0;
            return;
        }
        self.entry.pop();
        if self.entry == "-" {
            self.entry.clear();
        }
        if self.entry.is_empty() {
            self.result = 0.0;
        }
    }

    fn operator(&mut self, op: Op) {
        // Two operators in a row: the second one wins, and nothing is
        // computed. `7 + ×` is `7 ×`.
        if self.awaiting && self.pending.is_some() {
            self.pending = Some(op);
            self.history = format!("{} {}", format(self.acc), op.symbol());
            return;
        }
        let v = self.value();
        let lhs = match self.pending.take() {
            Some(p) => match p.apply(self.acc, v) {
                Some(r) => r,
                None => return self.fail(),
            },
            None => v,
        };
        self.acc = lhs;
        self.result = lhs;
        self.entry.clear();
        self.pending = Some(op);
        self.awaiting = true;
        self.history = format!("{} {}", format(lhs), op.symbol());
    }

    fn equals(&mut self) {
        let v = self.value();
        if let Some(p) = self.pending.take() {
            self.history = format!("{} {} {} =", format(self.acc), p.symbol(), format(v));
            match p.apply(self.acc, v) {
                Some(r) => self.result = r,
                None => return self.fail(),
            }
        } else {
            // `=` with nothing pending just settles what is on show.
            self.result = v;
            self.history = format!("{} =", format(v));
        }
        self.acc = self.result;
        self.entry.clear();
        self.awaiting = false;
    }

    /// Enter the error state, keeping the history that led to it.
    fn fail(&mut self) {
        self.error = true;
        self.entry.clear();
        self.pending = None;
        self.awaiting = false;
        self.result = 0.0;
        self.acc = 0.0;
    }

    /// Significant digits typed so far: the sign, the point and any
    /// **leading** zeros do not count, so `0.00123` is three digits and
    /// not six. That is the arithmetic meaning of the word, and using any
    /// other one would stop `0.1` after one keystroke.
    fn significant(&self) -> usize {
        let digits: String = self.entry.chars().filter(char::is_ascii_digit).collect();
        digits.trim_start_matches('0').len()
    }

    fn refresh(&mut self) {
        self.display = if self.error {
            "Error".to_owned()
        } else if self.entry.is_empty() {
            format(self.result)
        } else {
            self.entry.clone()
        };
    }
}

/// Render `v` the way a calculator should: [`DIGITS`] significant digits,
/// no trailing zeros, and no surprises from the binary representation.
///
/// `{}` on an `f64` prints the shortest string that round-trips, which is
/// exactly the wrong thing here: it is how `0.1 + 0.2` becomes
/// `0.30000000000000004`. So the value is rounded **once**, to 15
/// significant digits, by asking the formatter for scientific notation
/// with 14 decimals — and the fixed-point form is then assembled from
/// those digits by moving the point, rather than formatted a second time,
/// because rounding twice is how a digit that was correct stops being so.
#[must_use]
pub fn format(v: f64) -> String {
    if !v.is_finite() {
        return "Error".to_owned();
    }
    if v == 0.0 {
        // Catches -0.0, which is a real `f64` and a silly thing to show.
        return "0".to_owned();
    }
    let sci = format!("{v:.*e}", DIGITS - 1);
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };

    // Fixed notation while it stays readable: 15 integer digits is the
    // most that can be printed without inventing precision, and 1e-6 is
    // where leading zeros start to outnumber the digits.
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if (-6..=MAX_FIXED_EXP).contains(&exp) {
        if exp < 0 {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(digits);
        } else {
            let int_len = (exp + 1) as usize;
            for (i, c) in digits.chars().enumerate() {
                if i == int_len {
                    out.push('.');
                }
                out.push(c);
            }
            // Fewer digits than the exponent asks for: pad the integer
            // part, which is where `1e14` gets its fourteen zeros.
            for _ in digits.len()..int_len {
                out.push('0');
            }
        }
    } else {
        let mut chars = digits.chars();
        out.push(chars.next().unwrap_or('0'));
        let rest: String = chars.collect();
        if !rest.is_empty() {
            out.push('.');
            out.push_str(&rest);
        }
        out.push('e');
        if exp >= 0 {
            out.push('+');
        }
        out.push_str(&exp.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type a string of keys: the same characters a user would press.
    fn run(keys: &str) -> Engine {
        let mut e = Engine::new();
        for c in keys.chars() {
            match c {
                '\n' => e.press(Key::Equals),
                '<' => e.press(Key::Backspace),
                '~' => e.press(Key::Negate),
                _ => {
                    if let Some(k) = Key::from_text(&c.to_string()) {
                        e.press(k);
                    } else {
                        panic!("no key for {c:?}");
                    }
                }
            }
        }
        e
    }

    fn display(keys: &str) -> String {
        run(keys).display().to_owned()
    }

    #[test]
    fn a_fresh_calculator_shows_zero() {
        let e = Engine::new();
        assert_eq!(e.display(), "0");
        assert_eq!(e.history(), "");
        assert!(!e.is_error());
    }

    #[test]
    fn digits_accumulate_and_a_leading_zero_is_a_placeholder() {
        assert_eq!(display("7"), "7");
        assert_eq!(display("07"), "7");
        assert_eq!(display("123"), "123");
        // The display is the string as typed, trailing point and all —
        // which is what tells the user the point was accepted.
        assert_eq!(display("3."), "3.");
        assert_eq!(display("3.5"), "3.5");
        assert_eq!(display(".5"), "0.5");
        assert_eq!(display("3..5"), "3.5", "a second point is ignored");
    }

    #[test]
    fn entry_stops_at_fifteen_significant_digits() {
        assert_eq!(display("1234567890123456789"), "123456789012345");
        assert_eq!(
            display("0.12345678901234567"),
            "0.123456789012345",
            "the leading zero is not a significant digit"
        );
        assert_eq!(
            display("0.000123456789012345678"),
            "0.000123456789012345",
            "and neither are the zeros after the point"
        );
        // The backstop stops an entry of nothing but zeros growing for
        // ever — significant digits alone would never reach the limit.
        let e = run(&format!("0.{}", "0".repeat(40)));
        assert!(e.display().len() <= 24, "bounded: {:?}", e.display());
    }

    #[test]
    fn addition_and_the_history_line() {
        let e = run("7+8\n");
        assert_eq!(e.display(), "15");
        assert_eq!(e.history(), "7 + 8 =");
    }

    #[test]
    fn operations_chain_left_to_right_like_a_desk_calculator() {
        // Not 14: each operator completes what is pending. Documented at
        // the top of this module, and the reason there are no brackets.
        assert_eq!(display("2+3*4\n"), "20");
        assert_eq!(display("100/4-5\n"), "20");
        // An operator shows the running total before the next operand.
        let e = run("2+3*");
        assert_eq!(e.display(), "5", "the operator key computes what is due");
        assert_eq!(e.history(), "5 ×");
    }

    #[test]
    fn two_operators_in_a_row_keep_the_last_one() {
        let e = run("7+*3\n");
        assert_eq!(e.display(), "21");
        assert_eq!(e.history(), "7 × 3 =");
    }

    #[test]
    fn a_digit_after_a_result_starts_a_new_number() {
        let e = run("7+8\n9");
        assert_eq!(e.display(), "9");
        let e = run("7+8\n9+1\n");
        assert_eq!(e.display(), "10");
    }

    #[test]
    fn an_operator_after_a_result_carries_it_forward() {
        let e = run("7+8\n+5\n");
        assert_eq!(e.display(), "20");
    }

    #[test]
    fn equals_with_nothing_pending_settles_the_entry() {
        let e = run("42\n");
        assert_eq!(e.display(), "42");
        assert_eq!(e.history(), "42 =");
    }

    #[test]
    fn division_by_zero_is_an_error_that_only_clear_escapes() {
        let mut e = run("8/0\n");
        assert!(e.is_error());
        assert_eq!(e.display(), "Error");
        assert_eq!(e.history(), "8 ÷ 0 =", "the history says what failed");

        // Everything is refused while the error stands.
        e.press(Key::Digit(5));
        e.press(Key::Op(Op::Add));
        assert_eq!(e.display(), "Error");

        e.press(Key::Clear);
        assert_eq!(e.display(), "0");
        assert!(!e.is_error());
        assert_eq!(e.history(), "");

        // And it computes again afterwards.
        e.press(Key::Digit(1));
        e.press(Key::Op(Op::Add));
        e.press(Key::Digit(1));
        e.press(Key::Equals);
        assert_eq!(e.display(), "2");
    }

    #[test]
    fn backspace_also_recovers_from_an_error() {
        let mut e = run("1/0\n");
        assert!(e.is_error());
        e.press(Key::Backspace);
        assert_eq!(e.display(), "0");
        assert!(!e.is_error());
    }

    #[test]
    fn an_overflow_is_an_error_rather_than_infinity() {
        // Fifteen nines squared is 30 digits, so the display switches to
        // exponential: big, but perfectly representable.
        let mut e = Engine::new();
        for _ in 0..15 {
            e.press(Key::Digit(9));
        }
        e.press(Key::Op(Op::Mul));
        e.press(Key::Equals);
        assert_eq!(e.display(), "9.99999999999998e+29");
        assert!(!e.is_error());

        // Keep squaring and it leaves the range of an `f64` entirely,
        // which is an `Error` rather than `inf` on the screen.
        for _ in 0..8 {
            e.press(Key::Op(Op::Mul));
            e.press(Key::Equals);
            if e.is_error() {
                break;
            }
        }
        assert!(e.is_error(), "repeated squaring overflows");
        assert_eq!(e.display(), "Error");
    }

    #[test]
    fn backspace_drops_one_character_at_a_time() {
        assert_eq!(display("123<"), "12");
        assert_eq!(display("123<<"), "1");
        assert_eq!(display("123<<<"), "0");
        assert_eq!(display("1.5<"), "1.");
        assert_eq!(display("7+8\n<"), "0", "with nothing typed it clears");
    }

    #[test]
    fn negate_flips_the_entry_and_the_result() {
        assert_eq!(display("5~"), "-5");
        assert_eq!(display("5~~"), "5");
        assert_eq!(display("0~"), "0", "there is no negative zero to type");
        assert_eq!(display("7+8\n~"), "-15");
        assert_eq!(display("5~+3\n"), "-2");
    }

    #[test]
    fn clear_forgets_the_pending_operation_too() {
        let mut e = run("7+");
        e.press(Key::Clear);
        assert_eq!(e.display(), "0");
        assert_eq!(e.history(), "");
        e.press(Key::Digit(3));
        e.press(Key::Equals);
        assert_eq!(e.display(), "3", "the + did not survive the C");
    }

    #[test]
    fn a_typed_key_means_the_same_as_the_button() {
        assert_eq!(Key::from_text("7"), Some(Key::Digit(7)));
        assert_eq!(Key::from_text("*"), Some(Key::Op(Op::Mul)));
        assert_eq!(Key::from_text("x"), Some(Key::Op(Op::Mul)));
        assert_eq!(Key::from_text("×"), Some(Key::Op(Op::Mul)));
        assert_eq!(Key::from_text("/"), Some(Key::Op(Op::Div)));
        assert_eq!(Key::from_text(","), Some(Key::Dot), "a comma is a point");
        assert_eq!(Key::from_text("="), Some(Key::Equals));
        assert_eq!(Key::from_text("C"), Some(Key::Clear));
        assert_eq!(Key::from_text("q"), None, "q is the app's, not ours");
        assert_eq!(Key::from_text("ab"), None);
        assert_eq!(Key::from_text(""), None);
    }

    // -- the formatter ---------------------------------------------------

    #[test]
    fn the_formatter_hides_binary_floating_point() {
        // The headline case: `{}` would print 0.30000000000000004.
        assert_eq!(format(0.1 + 0.2), "0.3");
        assert_eq!(format(1.0 - 0.9), "0.1");
        assert_eq!(display("0.1+0.2\n"), "0.3");
    }

    #[test]
    fn the_formatter_keeps_fifteen_significant_digits() {
        assert_eq!(format(1.0 / 3.0), "0.333333333333333");
        assert_eq!(format(2.0 / 3.0), "0.666666666666667");
        assert_eq!(format(123_456_789.123_456_79), "123456789.123457");
    }

    #[test]
    fn the_formatter_drops_trailing_zeros_and_the_point() {
        assert_eq!(format(1.0), "1");
        assert_eq!(format(1.5), "1.5");
        assert_eq!(format(1.50), "1.5");
        assert_eq!(format(100.0), "100");
        assert_eq!(format(0.5), "0.5");
    }

    #[test]
    fn the_formatter_handles_zero_and_signs() {
        assert_eq!(format(0.0), "0");
        assert_eq!(format(-0.0), "0", "negative zero is still zero");
        assert_eq!(format(-1.5), "-1.5");
        assert_eq!(format(f64::NAN), "Error");
        assert_eq!(format(f64::INFINITY), "Error");
    }

    #[test]
    fn the_formatter_switches_to_exponential_at_the_edges() {
        assert_eq!(format(1e14), "100000000000000");
        assert_eq!(format(1e15), "1e+15");
        assert_eq!(format(1.5e20), "1.5e+20");
        assert_eq!(format(-2.5e-9), "-2.5e-9");
        assert_eq!(format(1e-6), "0.000001");
        assert_eq!(format(1e-7), "1e-7");
        assert_eq!(format(123.456e12), "123456000000000");
    }

    #[test]
    fn every_formatted_value_reads_back_as_itself() {
        // The round trip is what makes the formatter safe to feed to
        // `parse` — which is what the entry does with what it displays.
        // `f64::MAX` is deliberately not in the list: rounding it to 15
        // significant digits rounds it *up*, out of the range, and the
        // engine never shows it because an overflow is an `Error` before
        // it can be formatted.
        for v in [
            1.0,
            -2.5,
            1.0 / 3.0,
            0.1 + 0.2,
            1e14,
            1.5e20,
            2.5e-9,
            f64::MIN_POSITIVE,
            1e307,
        ] {
            let s = format(v);
            let back: f64 = s.parse().expect(&s);
            let rel = ((back - v) / v).abs();
            assert!(rel < 1e-14, "{s} parsed back to {back}, not {v}");
        }
    }
}
