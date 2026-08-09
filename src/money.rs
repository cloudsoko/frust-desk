/// The month picker's options: last 18 months. Tier-0 by construction —
/// the UI offers equality buckets, never a range control.
/// **The money-formatting ruling: pad a stored decimal to scale, for
/// DISPLAY only.**
///
/// SurrealDB strips trailing zeros at write, so `37.50` is stored — and read
/// back — as `37.5`. That is faithful storage and the no-arithmetic rule is
/// untouched by it;
/// what it is not is what a customer should read on an invoice.
///
/// Padding is not arithmetic: it appends zeros to a string. The stored value is
/// never touched, nothing is parsed into a float, and no total is recomputed —
/// which is exactly why the ruling permits it as presentation.
///
/// **A value with MORE places than the scale is returned VERBATIM, never
/// rounded.** The ruling is explicit: over-scale money is a defect to surface,
/// not to silently tidy. Money is stored *at* scale, so `1.005` in a 2-place
/// field means something upstream is wrong, and a display layer that quietly
/// printed `1.01` would hide it at the exact moment someone could still see it.
/// **Exact decimal subtraction, for a DERIVED report column.**
///
/// The AR report exists to answer "what does this customer owe", which is
/// `charged - paid`. That is a subtraction, and the one thing it must never be
/// is a float: this project has killed the float-money defect repeatedly
/// (Currency once mapped `TYPE float`; explicit rounding rules; one answer
/// across every host), and a financial report quietly doing
/// `300.0 - 120.0` in f64 would reintroduce it at the last mile.
///
/// **Where this computes:** in the
/// **Desk**, on the decimal *strings* the kernel sent, via scaled integers —
/// no float type is constructed at any point. Presentation-derived, never
/// stored, so the compare-never-compute rule is untouched (the report shows a
/// number; it writes nothing).
///
/// **A finding this exposes, recorded rather than papered over:** the Desk
/// has no shared decimal. `decimal.rs` lives in the kernel and is compiled into
/// the script sandbox, so this is a THIRD implementation of decimal
/// arithmetic in the codebase, and the standing lesson is that three hosts
/// must give one answer. This one is deliberately the smallest thing that can
/// be correct — subtraction at a fixed scale, nothing else — but the honest
/// long-term home is a kernel report path (or an exposed decimal).
///
/// Returns `None` when either side isn't a plain decimal, so the caller shows
/// nothing rather than a wrong number.
pub(crate) fn money_sub(a: &str, b: &str, scale: usize) -> Option<String> {
    let scaled = |raw: &str| -> Option<i128> {
        let t = raw.trim();
        if t.is_empty() {
            return None;
        }
        let (neg, digits) = match t.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let (int, frac) = match digits.split_once('.') {
            Some((i, f)) => (i, f),
            None => (digits, ""),
        };
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        // More places than the scale is REFUSED, not rounded — the same
        // posture `pad_money` takes. Silently dropping a place in a money
        // subtraction is precisely the defect class this guards.
        if frac.len() > scale {
            return None;
        }
        let mut n: i128 = int.parse().ok()?;
        for _ in 0..scale {
            n = n.checked_mul(10)?;
        }
        let mut f: i128 = if frac.is_empty() {
            0
        } else {
            frac.parse().ok()?
        };
        for _ in 0..(scale - frac.len()) {
            f = f.checked_mul(10)?;
        }
        let total = n.checked_add(f)?;
        Some(if neg { -total } else { total })
    };

    let diff = scaled(a)?.checked_sub(scaled(b)?)?;
    let neg = diff < 0;
    let mag = diff.unsigned_abs();
    let unit = 10u128.checked_pow(u32::try_from(scale).ok()?)?;
    let (whole, frac) = (mag / unit, mag % unit);
    Some(format!(
        "{}{}.{:0width$}",
        if neg { "-" } else { "" },
        whole,
        frac,
        width = scale
    ))
}

pub(crate) fn pad_money(raw: &str, scale: usize) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    // Only plain decimals are padded. Anything else (a currency symbol, an
    // expression, text in a mistyped field) is passed through untouched rather
    // than half-formatted.
    let body = t.strip_prefix('-').unwrap_or(t);
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int.is_empty() && frac.is_empty() {
        return t.to_string();
    }
    if !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return t.to_string();
    }
    if frac.len() >= scale {
        return t.to_string(); // already at scale, or over it — surface, don't round
    }
    let mut out = t.to_string();
    if frac.is_empty() {
        out.push('.');
    }
    out.push_str(&"0".repeat(scale - frac.len()));
    out
}

/// The scale a Currency field displays at.
///
/// **Two, always, today** — DocType metadata carries no `precision`, so there is
/// nothing per-field to read. Named rather than hidden: a per-field scale (and
/// a currency symbol) is print-metadata vocabulary for a later follow-on,
/// not something to hardcode per doctype here.
pub(crate) const MONEY_SCALE: usize = 2;

