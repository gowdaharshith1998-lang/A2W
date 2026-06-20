//! Vault encrypt/decrypt throughput.

use a2w_store::{Store, Vault};
use criterion::{criterion_group, criterion_main, Criterion};

fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (store, vault) = rt.block_on(async {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        // Test key: NOT all-equal (would be rejected); a simple non-trivial pattern.
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31).wrapping_add(7);
        }
        let v = Vault::try_new(key).unwrap();
        (store, v)
    });

    c.bench_function("vault/encrypt_512B", |b| {
        let plaintext = "x".repeat(512);
        b.iter(|| {
            rt.block_on(async {
                vault
                    .store_secret(&store, "bench", "Bench", &plaintext)
                    .await
                    .unwrap();
            })
        })
    });
    c.bench_function("vault/decrypt", |b| {
        b.iter(|| {
            rt.block_on(async {
                let _ = vault.get_secret(&store, "bench").await.unwrap();
            })
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
