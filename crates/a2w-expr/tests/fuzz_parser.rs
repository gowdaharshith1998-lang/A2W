//! Property-based fuzz of the expression parser + evaluator.
//!
//! Confirms `eval_str` and `render` never panic on arbitrary inputs (the
//! Audit-3 invariant for any untrusted-input parser).

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// Arbitrary byte strings up to 2 KiB never panic the parser.
    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let s = String::from_utf8_lossy(&bytes);
        let _ = a2w_expr::eval_str(&s, &serde_json::Value::Null);
        let _ = a2w_expr::render(&s, &serde_json::Value::Null);
    }

    /// Plausibly-shaped expressions don't panic and either evaluate or return
    /// an error.
    #[test]
    fn shape_like_exprs_dont_panic(src in expr_strategy()) {
        let _ = a2w_expr::eval_str(&src, &serde_json::Value::Null);
    }
}

/// Generate expression-ish strings built from a small grammar of tokens —
/// not a true grammar, but biased toward exercising the parser branches.
fn expr_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(token_strategy(), 1..32).prop_map(|v| v.join(" "))
}

fn token_strategy() -> impl Strategy<Value = &'static str> {
    proptest::sample::select(
        [
            "$.name",
            "$.age",
            "$.items[0]",
            "$.a.b.c",
            "1",
            "2.5",
            "-3",
            "0",
            "100",
            "true",
            "false",
            "null",
            "\"hello\"",
            "''",
            "+",
            "-",
            "*",
            "/",
            "%",
            "==",
            "!=",
            "<",
            ">",
            "<=",
            ">=",
            "&&",
            "||",
            "!",
            "(",
            ")",
            ",",
            "length(",
            "contains(",
            "upper(",
            "lower(",
            "coalesce(",
            "if(",
            "to_string(",
            "to_number(",
            "not(",
        ]
        .as_slice(),
    )
}
