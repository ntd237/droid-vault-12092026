// T3.2 — Standalone test harness for src/packages.rs.
// lib.rs cannot declare `pub mod packages;` yet (Wave 4 owns lib.rs), so this
// integration test compiles packages.rs directly with its inline #[cfg(test)] tests.

#[path = "../src/packages.rs"]
mod packages;
