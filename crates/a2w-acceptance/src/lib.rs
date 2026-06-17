//! # a2w-acceptance
//!
//! This crate intentionally contains no library code. It exists solely to host
//! cross-crate, end-to-end **acceptance tests** (under `tests/`) that exercise
//! the whole A2W stack — IR, validator, engine, nodes, testkit, optimizer —
//! composed into the full agent loop. Keeping these tests in a dedicated,
//! leaf-position crate lets it dev-depend on every other crate without forming
//! a normal-dependency cycle.

#![forbid(unsafe_code)]
