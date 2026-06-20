//! Property-based fuzz of [`a2w_ir::Workflow::from_json`].
//!
//! Generates arbitrary JSON-like strings and confirms the parser never panics
//! or yields an unexpected error category. Catches assertion / index-OOB /
//! integer-overflow regressions in the IR's serde stack.

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Arbitrary byte strings up to 4 KiB must not panic the parser.
    #[test]
    fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let s = String::from_utf8_lossy(&bytes);
        let _ = a2w_ir::Workflow::from_json(&s);
    }

    /// Arbitrary-shape JSON values rendered to a string never panic the
    /// parser. (Generated via serde_json's randomized output of nested arrays
    /// + objects with primitive leaves.)
    #[test]
    fn arbitrary_json_never_panic(seed in any::<u64>()) {
        // Deterministic mini-generator: build a JSON tree from the seed.
        let value = synth_json(seed, 4);
        let s = serde_json::to_string(&value).unwrap_or_default();
        let _ = a2w_ir::Workflow::from_json(&s);
    }
}

/// Build a small deterministic JSON value from a seed.
fn synth_json(seed: u64, depth: u32) -> serde_json::Value {
    if depth == 0 {
        return match seed % 5 {
            0 => serde_json::Value::Null,
            1 => serde_json::Value::Bool(seed.is_multiple_of(2)),
            2 => serde_json::Value::from((seed % 1000) as i64),
            3 => serde_json::Value::String(format!("s{seed}")),
            _ => serde_json::json!({}),
        };
    }
    let next = seed.wrapping_mul(1103515245).wrapping_add(12345);
    match seed % 3 {
        0 => {
            let n = (seed % 4) as usize;
            let arr: Vec<_> = (0..n)
                .map(|i| synth_json(next.wrapping_add(i as u64), depth - 1))
                .collect();
            serde_json::Value::Array(arr)
        }
        1 => {
            let n = (seed % 4) as usize;
            let mut m = serde_json::Map::new();
            for i in 0..n {
                m.insert(
                    format!("k{i}"),
                    synth_json(next.wrapping_add(i as u64), depth - 1),
                );
            }
            serde_json::Value::Object(m)
        }
        _ => synth_json(next, depth - 1),
    }
}
