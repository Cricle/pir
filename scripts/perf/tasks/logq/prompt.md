`logq.js` is a skeleton for a dependency-free JSON-lines log query CLI.
`README.md` is the full specification. Implement it so these visible tests pass:

```bash
node --test
```

The tool must support:

1. reading one or more files (or stdin with no file / `-`), ignoring blank and
   `#` comment lines, and exiting 2 with `line <n>: invalid json` on bad input;
2. repeatable `--level` (case-insensitive) and `--service` filters;
3. `--since` / `--until` half-open time windows with timezone offsets;
4. case-insensitive `--grep` over `msg` only;
5. `--fields` projection with dotted paths and `null` for missing values;
6. `--format json` (default) and RFC4180 `--format csv`;
7. `--sort` (`-field` for descending, missing last) and positive `--limit`;
8. `--stats` and `--group-by` aggregation of finite numbers.

Usage errors exit 2 and write `logq: ...` to stderr. Keep it CommonJS and use
only the Node standard library. Do not add npm dependencies. All eight areas
are tested independently, so partial implementations earn partial credit.
