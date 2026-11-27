# Rustdoc gates

How documentation correctness is enforced, what is enabled today, and what
would have to be true to tighten it.

## What is on

`crates/pir-core/src/lib.rs` carries one lint:

```rust
#![warn(rustdoc::broken_intra_doc_links)]
```

The engine has thousands of ``[`crate::...`]`` references in doc comments
(module headers, type docs, phase notes). Without this lint, renaming or
moving a type leaves every reference silently dangling in the rendered docs —
a broken link renders as plain text, so nothing fails and the docs rot
quietly.

It is `warn`, not `deny`.

## Why `warn` and not `deny`

The check has not been exercised end to end in CI. The `lint` job
(`.github/workflows/ci.yml`) runs `cargo fmt`, `clippy --all-targets
--all-features`, the SDK no-default matrix, doctests and the offline examples —
but no `cargo doc`.

That matters because doctests and rustdoc links are different checks:

- `cargo test --doc` compiles and runs fenced examples.
- `rustdoc::broken_intra_doc_links` only fires while *rendering* docs, i.e.
  under `cargo doc`.

A tree can be green on doctests and still carry hundreds of dangling links.
Until a `cargo doc` run is green and wired into CI, `deny` would turn any
pre-existing link into a build break for every contributor at once.

## Tightening it

Two steps, in order:

1. Run `cargo doc -p pir-core --no-deps` locally (GNU toolchain on Windows,
   or the ubuntu CI runner) and clear the reported links.
2. Add a `cargo doc --workspace --no-deps` step to the `lint` job, then flip
   the attribute to `deny`.

Only after step 2 is green is the attribute change safe.

## Related

- [`extensions.md`](extensions.md) — the `pi`/`ctx` doc surface that extension
  authors read.
- [`sdk.md`](sdk.md) — the documented embedding surface; this is the API whose
  doc links matter most to external consumers.
- [`architecture.md`](architecture.md) — crate split, which decides where a
  doc comment physically lives.