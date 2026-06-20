//! Expression engine eval throughput.

use a2w_expr::eval_str;
use criterion::{criterion_group, criterion_main, Criterion};

fn bench(c: &mut Criterion) {
    let item = serde_json::json!({
        "name": "Alice",
        "age": 30,
        "tags": ["admin", "active"],
        "address": { "city": "NYC", "zip": "10001" }
    });

    c.bench_function("expr/path_lookup", |b| {
        b.iter(|| eval_str("$.address.city", &item).unwrap())
    });
    c.bench_function("expr/arithmetic_and_logic", |b| {
        b.iter(|| eval_str("$.age > 18 && length($.tags) >= 2", &item).unwrap())
    });
    c.bench_function("expr/string_concat", |b| {
        b.iter(|| eval_str("\"Hello, \" + $.name + \"!\"", &item).unwrap())
    });
    c.bench_function("expr/render_template", |b| {
        b.iter(|| a2w_expr::render("Hi ${{ $.name }} (${{ $.age }})", &item))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
