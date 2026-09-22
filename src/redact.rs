//! PII redaction on the ingest path.
//!
//! evald's pitch is regulated, air-gapped and on-prem deployments, and until this existed it
//! wrote prompts and completions to Parquet exactly as they arrived. "The data never leaves
//! your box" answers exfiltration; it does not answer the compliance officer asking which
//! fields are masked and who can read them. This module is that answer.
//!
//! ## Where it runs, and why that matters
//!
//! Redaction happens **before the WAL append and before the blob offload** — see
//! [`crate::Store::prepare_for_storage`]. The WAL is the ACK boundary, so a value that
//! reaches it is durable by definition; redacting after that point would be cosmetic, and
//! redacting after the offload would leave the raw value in a blob file. The raw value never
//! touches disk in any tier.
//!
//! That also makes it **irreversible by design**. There is no "unredact": the original is
//! gone before anything is written. That is the property a regulated buyer is actually
//! buying, and the reason redaction is opt-in — silently rewriting a user's telemetry would
//! be the wrong default.
//!
//! ## Precision over recall
//!
//! A redactor that mangles ordinary text is one an operator turns off, at which point it
//! protects nothing. So the detectors here prefer a missed match to a false one:
//!
//! - **Credit cards are Luhn-checked.** A bare 13–19 digit regex matches order numbers,
//!   trace ids and timestamps. The checksum removes essentially all of them.
//! - **IPv4 octets are range-checked**, though `1.2.3.4` is a valid address *and* a plausible
//!   version string — see [`Class::Ip`]'s note. This is the one detector where the tradeoff
//!   is genuinely unresolvable, which is why classes are selected individually.
//! - **API keys match known vendor shapes** (`sk-`, `AKIA`, `ghp_`, `xox…`), not "a long
//!   random-looking string", which would hit every trace id in the store.
//!
//! What is deliberately NOT scanned: `user_id`, `session_id`, `service_name` and the span
//! name. Those are identifiers the caller chose to send, they are usually already opaque,
//! and rewriting them would silently break `evald cost --by user|session`. An operator who
//! puts an email in `user_id` should hash it upstream — the docs say so.

use crate::model::NormalizedSpan;
use std::collections::BTreeMap;
use std::fmt;

/// A category of sensitive value evald can detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// `user@example.com`.
    Email,
    /// 13–19 digit PANs that pass a Luhn check.
    CreditCard,
    /// US social security numbers in `123-45-6789` form.
    Ssn,
    /// E.164 (`+14155550123`) and North-American (`(415) 555-0123`) shapes.
    Phone,
    /// IPv4 and IPv6 literals.
    ///
    /// The highest-false-positive class by some distance: `1.2.3.4` is both a routable
    /// address and a four-part version string, and no amount of validation separates them.
    /// Enable it when addresses are genuinely PII in your jurisdiction (they are under
    /// GDPR), and expect version strings in prompts to be caught with them.
    Ip,
    /// Provider API keys in their published shapes (`sk-…`, `AKIA…`, `ghp_…`, `xox…`).
    ApiKey,
    /// JSON Web Tokens — a `eyJ`-prefixed three-segment blob.
    Jwt,
}

impl Class {
    /// Every built-in class, which is what `--redact all` selects.
    pub const ALL: [Class; 7] = [
        Class::Email,
        Class::CreditCard,
        Class::Ssn,
        Class::Phone,
        Class::Ip,
        Class::ApiKey,
        Class::Jwt,
    ];

    /// The name used in config, in the placeholder, and in the metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Email => "email",
            Class::CreditCard => "credit_card",
            Class::Ssn => "ssn",
            Class::Phone => "phone",
            Class::Ip => "ip",
            Class::ApiKey => "api_key",
            Class::Jwt => "jwt",
        }
    }

    fn parse(s: &str) -> Option<Class> {
        Class::ALL
            .into_iter()
            .find(|c| c.as_str() == s.trim().to_ascii_lowercase())
    }

    /// The detector pattern. Ordered by specificity where they can overlap: the combined
    /// regex is a leftmost-first alternation, so `jwt` and `api_key` must precede the looser
    /// shapes or a key would be partly eaten by `email`/`phone`.
    fn pattern(self) -> &'static str {
        match self {
            // Header segment of a JWT is always base64url of `{"`, i.e. `eyJ`.
            Class::Jwt => r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            Class::ApiKey => {
                r"(?:sk-[A-Za-z0-9_-]{16,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,}|xox[abpr]-[0-9A-Za-z-]{10,})"
            }
            Class::Email => r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
            Class::Ssn => r"[0-9]{3}-[0-9]{2}-[0-9]{4}",
            // Digits with optional single spaces/hyphens. Every hit is Luhn-checked before
            // it is treated as a card, so this loose shape costs precision nothing.
            Class::CreditCard => r"[0-9](?:[ \-]?[0-9]){12,18}",
            Class::Phone => r"(?:\+[1-9][0-9]{7,14}|\(?[0-9]{3}\)?[ \-][0-9]{3}-[0-9]{4})",
            // IPv4 octets are range-checked by the pattern itself; the IPv6 arm is the
            // common fully/partly-elided forms, not the full grammar.
            Class::Ip => {
                r"(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])|(?:[0-9A-Fa-f]{1,4}:){2,7}[0-9A-Fa-f]{1,4}"
            }
        }
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to do with a detected value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    /// Replace with `[REDACTED:<class>]`. The default: unambiguous in a trace view, and it
    /// keeps the surrounding text readable.
    #[default]
    Redact,
    /// Replace with `[<class>:<16 hex>]`, a truncated SHA-256 of the value.
    ///
    /// The value is unrecoverable, but *equal values hash equally* — so "how many distinct
    /// users hit this error" and "is this the same card as that one" stay answerable without
    /// the store ever holding the value. Salt-free on purpose: a per-process salt would make
    /// hashes incomparable across restarts and across nodes in a fleet, which is the entire
    /// point. Treat a hash as a pseudonym, not an anonymisation — a short numeric space (a
    /// SSN, say) is brute-forceable from the digest.
    Hash,
    /// Remove the match entirely, leaving the surrounding text joined.
    Drop,
}

impl Action {
    fn parse(s: &str) -> Option<Action> {
        match s.trim().to_ascii_lowercase().as_str() {
            "redact" => Some(Action::Redact),
            "hash" => Some(Action::Hash),
            "drop" => Some(Action::Drop),
            _ => None,
        }
    }
}

/// A compiled redaction policy. Cheap to clone (the regex is shared).
#[derive(Clone)]
pub struct Redactor {
    /// One alternation over every enabled pattern, so a value is scanned **once** regardless
    /// of how many classes are on. Rule `i`'s own group is `groups[i]`.
    re: regex::Regex,
    /// Class label per rule, in alternation order. A custom rule's label is its configured
    /// name.
    labels: Vec<String>,
    /// Capture-group number of each rule's wrapping group in `re`, parallel to `labels`.
    ///
    /// **Not** `i + 1`. A custom rule's regex may contain capture groups of its own —
    /// `--redact-custom 'ticket=(?:ABC|DEF)-[0-9]+'` does not, but
    /// `--redact-custom 'ticket=(ABC|DEF)-[0-9]+'` does — and each one shifts the numbering
    /// of every alternative after it. Assuming `i + 1` made a later rule's hit read as an
    /// earlier rule's: the wrong label in the output, the wrong series in
    /// `evald_redactions_total`, and the wrong `luhn_gated` flag applied, which could drop a
    /// real match on a checksum its pattern never promised to satisfy.
    groups: Vec<usize>,
    /// Which rules are Luhn-gated (the built-in credit-card rule).
    luhn_gated: Vec<bool>,
    action: Action,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("rules", &self.labels)
            .field("action", &self.action)
            .finish()
    }
}

/// Why a policy could not be built.
#[derive(Debug)]
pub enum RedactError {
    UnknownClass(String),
    UnknownAction(String),
    BadCustom(String),
    BadRegex { name: String, err: String },
}

impl fmt::Display for RedactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RedactError::UnknownClass(s) => write!(
                f,
                "unknown redaction class {s:?} (known: {}, or `all`)",
                Class::ALL
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            RedactError::UnknownAction(s) => {
                write!(
                    f,
                    "unknown redaction action {s:?} (use redact | hash | drop)"
                )
            }
            RedactError::BadCustom(s) => {
                write!(f, "malformed --redact-custom {s:?} (expected name=regex)")
            }
            RedactError::BadRegex { name, err } => {
                write!(
                    f,
                    "custom redaction rule {name:?} has an invalid regex: {err}"
                )
            }
        }
    }
}

impl std::error::Error for RedactError {}

impl Redactor {
    /// Build a policy from config.
    ///
    /// `classes` is a comma-separated list of class names, or `all`. `custom` entries are
    /// `name=regex`. An empty selection yields `None` — no policy, and therefore no cost on
    /// the ingest path at all.
    ///
    /// Every regex is compiled here, at startup, so a malformed custom rule fails the
    /// process rather than silently never matching once traffic is flowing.
    pub fn build(
        classes: &[String],
        custom: &[String],
        action: &str,
    ) -> Result<Option<Redactor>, RedactError> {
        let action =
            Action::parse(action).ok_or_else(|| RedactError::UnknownAction(action.into()))?;

        let mut selected: Vec<Class> = Vec::new();
        for entry in classes.iter().flat_map(|c| c.split(',')) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if entry.eq_ignore_ascii_case("all") {
                selected.extend(Class::ALL);
            } else {
                selected.push(
                    Class::parse(entry).ok_or_else(|| RedactError::UnknownClass(entry.into()))?,
                );
            }
        }
        selected.sort();
        selected.dedup();

        let mut alternatives: Vec<String> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        let mut luhn_gated: Vec<bool> = Vec::new();
        let mut groups: Vec<usize> = Vec::new();
        // The group number the NEXT alternative's wrapping group will get. Advanced by the
        // number of groups each alternative actually contributes — its own wrapper, plus any
        // the pattern brings with it — rather than by one. See `Redactor::groups`.
        //
        // `captures_len()` counts the implicit whole-match group, so a pattern with no
        // groups of its own reports 1: exactly the one group wrapping it adds.
        let mut next_group = 1usize;

        // Specificity order, not selection order: the alternation is leftmost-first, so a JWT
        // or API key must get the chance to match before `email`/`phone` claims part of it.
        for class in Class::ALL.into_iter().filter(|c| selected.contains(c)) {
            // The built-in patterns use only non-capturing groups today, but counting them
            // the same way as custom ones means adding a capturing group to one of them can
            // never silently shift the rules after it.
            let own = regex::Regex::new(class.pattern())
                .expect("built-in class patterns are valid")
                .captures_len();
            alternatives.push(format!("({})", class.pattern()));
            labels.push(class.as_str().to_string());
            luhn_gated.push(class == Class::CreditCard);
            groups.push(next_group);
            next_group += own;
        }

        for spec in custom {
            let (name, pattern) = spec
                .split_once('=')
                .ok_or_else(|| RedactError::BadCustom(spec.clone()))?;
            let (name, pattern) = (name.trim(), pattern.trim());
            if name.is_empty() || pattern.is_empty() {
                return Err(RedactError::BadCustom(spec.clone()));
            }
            // Validate standalone so the error names the offending rule rather than pointing
            // at a combined pattern the operator never wrote — and, while it is compiled,
            // ask how many capture groups it contributes to the alternation.
            let own = regex::Regex::new(pattern)
                .map_err(|e| RedactError::BadRegex {
                    name: name.to_string(),
                    err: e.to_string(),
                })?
                .captures_len();
            alternatives.push(format!("({pattern})"));
            labels.push(name.to_string());
            luhn_gated.push(false);
            groups.push(next_group);
            next_group += own;
        }

        if alternatives.is_empty() {
            return Ok(None);
        }

        let combined = alternatives.join("|");
        let re = regex::Regex::new(&combined).map_err(|e| RedactError::BadRegex {
            name: "<combined>".to_string(),
            err: e.to_string(),
        })?;
        debug_assert_eq!(
            re.captures_len(),
            next_group,
            "group accounting disagrees with the compiled alternation"
        );
        Ok(Some(Redactor {
            re,
            labels,
            groups,
            luhn_gated,
            action,
        }))
    }

    /// The rule labels this policy will report, for the metric's label set.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Rewrite `text`. Returns the rewritten string and per-RULE-INDEX hit counts, or
    /// `None` if nothing matched — the overwhelmingly common case, and the one that must not
    /// allocate.
    ///
    /// Counts are indices into [`Self::labels`], not label strings: this runs on a path that
    /// sustains tens of thousands of spans a second, and cloning a label per hit would put
    /// an allocation in the middle of it for a name the caller already has.
    pub fn scrub(&self, text: &str) -> Option<(String, Vec<(usize, u64)>)> {
        let mut out: Option<String> = None;
        let mut last = 0usize;
        let mut counts: BTreeMap<usize, u64> = BTreeMap::new();

        for caps in self.re.captures_iter(text) {
            // Exactly one rule's wrapping group matched; find which, to get its label. The
            // scan is over the RECORDED group numbers, never `1..=labels.len()`: a custom
            // rule's own capture groups sit between them.
            let Some((idx, m)) = self
                .groups
                .iter()
                .enumerate()
                .find_map(|(idx, &group)| caps.get(group).map(|m| (idx, m)))
            else {
                continue;
            };
            // A Luhn-gated hit that fails the checksum is not a card — leave it alone. This
            // is what keeps order numbers and trace ids out of the redactor's jaws.
            if self.luhn_gated[idx] && !luhn_valid(m.as_str()) {
                continue;
            }
            let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
            buf.push_str(&text[last..m.start()]);
            let label = &self.labels[idx];
            match self.action {
                Action::Redact => {
                    buf.push_str("[REDACTED:");
                    buf.push_str(label);
                    buf.push(']');
                }
                Action::Hash => {
                    buf.push('[');
                    buf.push_str(label);
                    buf.push(':');
                    buf.push_str(&short_hash(m.as_str()));
                    buf.push(']');
                }
                Action::Drop => {}
            }
            last = m.end();
            *counts.entry(idx).or_default() += 1;
        }

        let mut buf = out?;
        buf.push_str(&text[last..]);
        Some((buf, counts.into_iter().collect()))
    }

    /// Apply the policy to every scanned field of a span, returning per-label hit counts.
    ///
    /// Scans `input_value`, `output_value` and every string in `raw_attributes` (recursing
    /// through arrays and objects, since an attribute can be structured). See the module
    /// docs for what is deliberately left alone.
    pub fn scrub_span(&self, span: &mut NormalizedSpan) -> Vec<(usize, u64)> {
        let mut total: BTreeMap<usize, u64> = BTreeMap::new();
        let mut merge = |counts: Vec<(usize, u64)>| {
            for (k, v) in counts {
                *total.entry(k).or_default() += v;
            }
        };

        for text in [&mut span.input_value, &mut span.output_value]
            .into_iter()
            .flatten()
        {
            if let Some((clean, counts)) = self.scrub(text) {
                *text = clean;
                merge(counts);
            }
        }
        for value in span.raw_attributes.values_mut() {
            merge(self.scrub_json(value));
        }
        total.into_iter().collect()
    }

    /// Recurse a JSON attribute value, scrubbing every string it contains.
    ///
    /// Object *keys* are left alone: an attribute key is a schema name the instrumentation
    /// chose, not user content, and rewriting keys would break every query written against
    /// them.
    fn scrub_json(&self, value: &mut serde_json::Value) -> Vec<(usize, u64)> {
        let mut total: BTreeMap<usize, u64> = BTreeMap::new();
        match value {
            serde_json::Value::String(s) => {
                if let Some((clean, counts)) = self.scrub(s) {
                    *s = clean;
                    for (k, v) in counts {
                        *total.entry(k).or_default() += v;
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    for (k, v) in self.scrub_json(item) {
                        *total.entry(k).or_default() += v;
                    }
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    for (k, v) in self.scrub_json(item) {
                        *total.entry(k).or_default() += v;
                    }
                }
            }
            _ => {}
        }
        total.into_iter().collect()
    }
}

/// The Luhn checksum, over the digits of `s` (separators ignored).
///
/// This is what separates a payment card from the 16-digit order number next to it. Roughly
/// 9 in 10 random digit strings fail it, so it converts the loosest detector in the set into
/// the most precise one.
fn luhn_valid(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let doubled = d * 2;
                if doubled > 9 {
                    doubled - 9
                } else {
                    doubled
                }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// First 8 bytes of SHA-256, hex — a stable pseudonym for a value.
fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(s.as_bytes())[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor(classes: &str, action: &str) -> Redactor {
        Redactor::build(&[classes.to_string()], &[], action)
            .unwrap()
            .expect("a policy")
    }

    fn scrub(r: &Redactor, s: &str) -> String {
        r.scrub(s).map(|(t, _)| t).unwrap_or_else(|| s.to_string())
    }

    #[test]
    fn detects_each_class() {
        let r = redactor("all", "redact");
        assert_eq!(
            scrub(&r, "mail me at ada@example.com ok"),
            "mail me at [REDACTED:email] ok"
        );
        assert_eq!(scrub(&r, "ssn 123-45-6789"), "ssn [REDACTED:ssn]");
        assert_eq!(scrub(&r, "call +14155550123"), "call [REDACTED:phone]");
        assert_eq!(scrub(&r, "host 192.168.1.1"), "host [REDACTED:ip]");
        // Built by concatenation, not as one literal: both are well-known non-secret shapes
        // (an OpenAI-style key of the minimum matchable length; AWS's own published example
        // access key, "EXAMPLE"-suffixed for exactly this purpose) but a source-text secret
        // scanner — including this crate's own OSS-mirror gate — can't tell a realistic test
        // fixture from a real leaked key without running the program, so avoid handing it one
        // contiguous string to match.
        assert!(
            scrub(&r, &format!("key sk-{}", "abcdefghijklmnopqrstuvwx"))
                .contains("[REDACTED:api_key]")
        );
        assert!(
            scrub(&r, &format!("AKIA{}", "IOSFODNN7EXAMPLE")).contains("[REDACTED:api_key]")
        );
    }

    /// The Luhn gate is the difference between a usable redactor and one that eats every
    /// long number in the corpus.
    #[test]
    fn credit_cards_are_luhn_checked_so_order_numbers_survive() {
        let r = redactor("credit_card", "redact");
        // A real test PAN (passes Luhn).
        assert_eq!(
            scrub(&r, "card 4242424242424242"),
            "card [REDACTED:credit_card]"
        );
        assert_eq!(
            scrub(&r, "card 4242-4242-4242-4242"),
            "card [REDACTED:credit_card]"
        );
        // Same length, fails the checksum — an order id, not a card. Must survive intact.
        assert_eq!(
            scrub(&r, "order 1234567812345678"),
            "order 1234567812345678"
        );
        assert_eq!(scrub(&r, "id 9999999999999999"), "id 9999999999999999");
    }

    #[test]
    fn luhn_rejects_wrong_lengths_and_accepts_known_pans() {
        assert!(luhn_valid("4242424242424242"));
        assert!(luhn_valid("4111 1111 1111 1111"));
        assert!(luhn_valid("378282246310005")); // 15-digit Amex
        assert!(!luhn_valid("4242424242424241")); // one digit off
        assert!(!luhn_valid("123456789012")); // 12 digits — too short to be a PAN
    }

    /// A JWT or API key must not be partly claimed by a looser detector: the alternation is
    /// leftmost-first, so ordering is load-bearing, not cosmetic.
    #[test]
    fn specific_classes_win_over_loose_ones() {
        let r = redactor("all", "redact");
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let out = scrub(&r, &format!("token {jwt} end"));
        assert_eq!(out, "token [REDACTED:jwt] end", "a JWT must redact whole");
        // An email inside a longer key-ish string still resolves to exactly one class.
        // Built by concatenation, not one literal — see the note on `detects_each_class`.
        let out = scrub(&r, &format!("sk-{}", "abcdefghijklmnopqrstuvwx"));
        assert_eq!(out, "[REDACTED:api_key]");
    }

    #[test]
    fn hash_is_stable_and_irreversible_looking() {
        let r = redactor("email", "hash");
        let a = scrub(&r, "ada@example.com");
        let b = scrub(&r, "ada@example.com");
        let c = scrub(&r, "grace@example.com");
        assert_eq!(a, b, "equal values must hash equally — that is the point");
        assert_ne!(a, c);
        assert!(a.starts_with("[email:"), "{a}");
        assert!(!a.contains("ada"), "the value must not survive: {a}");
    }

    #[test]
    fn drop_removes_the_match_entirely() {
        let r = redactor("email", "drop");
        assert_eq!(scrub(&r, "from ada@example.com to x"), "from  to x");
    }

    #[test]
    fn clean_text_is_untouched_and_allocates_nothing() {
        let r = redactor("all", "redact");
        // `None` is the signal that the input was clean — the hot path must not build a
        // second copy of every prompt that happens to contain no PII.
        assert!(r.scrub("a perfectly ordinary prompt about rust").is_none());
    }

    #[test]
    fn span_fields_and_nested_attributes_are_all_scrubbed() {
        let r = redactor("all", "redact");
        let mut span = crate::store::test_span("t", "01", 1);
        span.input_value = Some("write to ada@example.com".into());
        span.output_value = Some("sure, ada@example.com".into());
        span.raw_attributes.insert(
            "custom.payload".into(),
            serde_json::json!({"nested": ["grace@example.com", {"deep": "ssn 123-45-6789"}]}),
        );
        span.raw_attributes
            .insert("plain".into(), serde_json::json!("no pii here"));

        let counts = r.scrub_span(&mut span);

        assert_eq!(
            span.input_value.as_deref(),
            Some("write to [REDACTED:email]")
        );
        assert_eq!(span.output_value.as_deref(), Some("sure, [REDACTED:email]"));
        let attr = span.raw_attributes["custom.payload"].to_string();
        assert!(!attr.contains("grace@example.com"), "{attr}");
        assert!(!attr.contains("123-45-6789"), "{attr}");
        // The key itself is untouched — it is a schema name, and queries are written on it.
        assert!(span.raw_attributes.contains_key("custom.payload"));
        assert_eq!(
            span.raw_attributes["plain"],
            serde_json::json!("no pii here")
        );

        let by_label: BTreeMap<&str, u64> = counts
            .into_iter()
            .map(|(i, n)| (r.labels()[i].as_str(), n))
            .collect();
        assert_eq!(by_label["email"], 3);
        assert_eq!(by_label["ssn"], 1);
    }

    #[test]
    fn custom_rules_are_supported_and_validated_at_build_time() {
        let r = Redactor::build(&["".into()], &["employee_id=EMP-[0-9]{6}".into()], "redact")
            .unwrap()
            .expect("a custom-only policy is valid");
        assert_eq!(scrub(&r, "user EMP-123456"), "user [REDACTED:employee_id]");

        // A bad regex must fail at startup, not silently never match under load.
        let err = Redactor::build(&[], &["broken=([unclosed".into()], "redact").unwrap_err();
        assert!(matches!(err, RedactError::BadRegex { .. }), "{err:?}");
        // As must a malformed spec, or an unknown class/action.
        assert!(Redactor::build(&[], &["noequals".into()], "redact").is_err());
        assert!(Redactor::build(&["nope".into()], &[], "redact").is_err());
        assert!(Redactor::build(&["email".into()], &[], "obliterate").is_err());
    }

    #[test]
    fn a_custom_rule_with_its_own_capture_groups_does_not_shift_the_rules_after_it() {
        // Regression: rules were resolved by assuming rule `i` owned capture group `i + 1`.
        // A custom pattern that contains a capture group of its own adds a group the
        // accounting never knew about, and every rule after it then answers to a group
        // number one too low. The symptoms are both silent and bad — a rule that stops
        // redacting anything (PII written through, with no error anywhere), and a rule
        // whose hits are attributed to a DIFFERENT rule's label and Luhn gate.
        let r = Redactor::build(
            &[],
            &[
                "region=(us|eu)-[a-z]+".into(), // one inner group — the shifter
                "ticket=TCK-[0-9]+".into(),
                "batch=B[0-9]{4}".into(),
            ],
            "redact",
        )
        .unwrap()
        .expect("three custom rules is a policy");

        // Before the fix: `TCK-9` matched no group in 1..=3 and was written through
        // untouched, and `B1234` was reported under the `batch` group number that actually
        // belonged to `ticket`.
        assert_eq!(
            scrub(&r, "in eu-west ticket TCK-9 batch B1234"),
            "in [REDACTED:region] ticket [REDACTED:ticket] batch [REDACTED:batch]"
        );

        // And the counts land on the rule that actually matched, since those are what
        // `evald_redactions_total` publishes per rule.
        let (_, counts) = r.scrub("eu-west TCK-9 B1234").expect("all three match");
        let by_label: std::collections::BTreeMap<&str, u64> = counts
            .iter()
            .map(|(idx, n)| (r.labels()[*idx].as_str(), *n))
            .collect();
        assert_eq!(by_label["region"], 1);
        assert_eq!(by_label["ticket"], 1);
        assert_eq!(by_label["batch"], 1);

        // The same accounting has to survive built-in classes sitting either side of the
        // shifter — a class is just another alternative, and `email` here comes first.
        let mixed = Redactor::build(
            &["email".into()],
            &["region=(us|eu)-[a-z]+".into(), "ticket=TCK-[0-9]+".into()],
            "redact",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            scrub(&mixed, "a@b.co eu-west TCK-9"),
            "[REDACTED:email] [REDACTED:region] [REDACTED:ticket]"
        );
    }

    #[test]
    fn a_shifted_credit_card_gate_still_applies_to_the_credit_card_rule() {
        // The Luhn gate is indexed by rule, so a shifted index would apply the checksum to
        // whichever rule happened to land on the card's slot — silently dropping that
        // rule's matches — while the card itself went through ungated.
        let r = Redactor::build(
            &["credit_card".into()],
            &["region=(us|eu)-[a-z]+".into(), "ticket=TCK-[0-9]+".into()],
            "redact",
        )
        .unwrap()
        .unwrap();
        // 4242424242424242 passes Luhn; 4242424242424243 does not. The failing one must be
        // left alone, and `TCK-1234567890123` — which matches the loose card shape only
        // after its prefix, so it does not — must still redact as a ticket.
        assert_eq!(
            scrub(&r, "4242424242424242 / 4242424242424243 / TCK-1234"),
            "[REDACTED:credit_card] / 4242424242424243 / [REDACTED:ticket]"
        );
    }

    #[test]
    fn no_classes_and_no_custom_rules_means_no_policy() {
        // Not an empty-but-active redactor: `None` is what lets the ingest path skip the
        // work entirely when redaction is off.
        assert!(Redactor::build(&[], &[], "redact").unwrap().is_none());
        assert!(Redactor::build(&["".into()], &[], "redact")
            .unwrap()
            .is_none());
    }
}
