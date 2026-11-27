# The verification floor (`--check`)

A plain `normal` run had no correctness guarantee: a tool error was a result
and the loop continued. `--check` adds one — the run is not complete until the
command you declared has actually been executed and succeeded.

```bash
pir -p "fix the duration parser" --check "cargo test duration"
pir -c --check "cargo test" --check "cargo clippy -- -D warnings"
```

## What it does

The declared command becomes the run's execution contract, which is the same
gate the plan lane uses:

- **The command must actually run.** A tool result with a zero exit status is
  required. Saying "verified" or reasoning about correctness does not count.
- **It must run after the last mutation.** Verify, then edit, then claim done
  is rejected — evidence only counts when nothing changed after it.
- **It must match verbatim.** The executed command string has to equal one of
  the declared strings (trimmed). `cargo test` does not satisfy
  `cargo test --all`. The prompt therefore states the exact command.
- **It is bounded.** Three nudges, then the run may finish but is reported
  **UNVERIFIED** rather than as verified work.

## Why a flag, not a setting

A verifier is per project — `cargo test` in this repo, `npm test` in the next —
so it is not a machine-wide preference. It persists **with the session**: pass
it again on `-c`/`--resume` and it replaces the stored list; without it, the
session keeps the checks it was started with. There is no `settings.json` key,
deliberately (the 67 flat keys are already too many).

## What it does not do

- It does not choose the command for you. `--check` is the source of truth;
  the model cannot pick a weaker check and pass.
- It does not gate `foreman` or `plan`. Foreman has its own independent
  reviewer; plan installs the approved plan's contract on approval. The
  declared-check floor arms only in a plain `normal` run, and never replaces a
  real plan contract when one is active.
- It does not affect runs with no `--check`. With nothing declared, behaviour
  is byte-for-byte what it was before the floor existed.