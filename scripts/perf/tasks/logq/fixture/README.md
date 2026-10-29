# logq

Query JSON-lines logs. Dependency-free Node.js CLI; `logq.js` is a skeleton.

```bash
node logq.js [options] [file ...]
```

If no file is given, or a file argument is `-`, stdin is read. Multiple inputs
are processed in order. Blank/whitespace-only lines and lines whose first
non-whitespace character is `#` are ignored. Every other line must be a JSON
object (not an array, not `null`); otherwise the CLI prints
`logq: line <n>: invalid json` (1-based line number across all inputs,
including ignored lines) and exits 2.

Records are arbitrary JSON objects, e.g.:

```json
{"ts":"2024-05-01T10:00:00Z","level":"error","service":"api","msg":"boom","latency_ms":25,"user":{"id":7}}
```

Field paths use dots (`user.id`). A missing path resolves to `null` in output.

## Options

### `--level <name>` (repeatable)

Keep records whose `level` string equals any given name, case-insensitively.
Missing/non-string `level` never matches.

### `--service <name>` (repeatable)

Keep records whose `service` string equals any given name exactly. Values OR.

### `--since <iso>` / `--until <iso>`

Parse with `Date.parse`; a non-finite result is a usage error. Keep records
with a parseable `ts` where `ts >= since` and/or `ts < until` (half-open).
Records with missing or unparseable `ts` are excluded whenever a time filter is
present. `since` and `until` may be combined.

### `--grep <regex>`

Case-insensitive JavaScript regular expression matched against `msg` only.
Non-string `msg` never matches. An invalid pattern is a usage error.

### `--fields <path[,path...]>`

Project each record to the listed paths, in order. The output key is the path
text itself (`user.id`), and a missing path becomes `null`. Empty path entries
are a usage error. Without `--fields`, JSON output contains the whole record.

### `--format json|csv`

Default `json`. JSON output is one compact object per record, one per line.
CSV requires `--fields` and emits a header row of the path names followed by
one row per record, `\n` line endings, RFC4180 quoting: quote when the value
contains `,`, `"`, `\r` or `\n`, and double embedded quotes. Missing/null
values are empty cells. Numbers/booleans use their text form; arrays/objects
are compact JSON.

### `--sort <field>` / `--limit <n>`

`--sort` accepts a leading `-` for descending. Missing/null values always sort
last. Two numbers compare numerically, two strings lexicographically, anything
else by `String(value)`. The sort is stable. `--limit` must be a positive
integer and is applied after sorting. Without `--sort`, input order is kept.

### `--stats <field>` / `--group-by <field>`

With `--stats`, do not print records. Collect finite numbers at the field path
and print one compact JSON object:

```json
{"count":3,"min":5,"max":20,"avg":11.666666666666666,"sum":35}
```

With no numeric values: `{"count":0,"min":null,"max":null,"avg":null,"sum":null}`.

With `--group-by`, print one JSON line per group, sorted ascending by group
value (missing/null last), where the group field is named by the leaf of the
path (`user.region` → `region`):

```json
{"service":"api","count":2,"min":10,"max":20,"avg":15,"sum":30}
```

Groups with no finite numeric values are still emitted with count 0 and null
aggregates. `--group-by` without `--stats` is a usage error. `--stats` ignores
`--fields` and `--format`; filters, sort and limit still apply.

### `--help`

Print usage to stdout and exit 0.

## Exit codes

`0` on success, `2` on any usage/input error. Errors go to stderr with the
prefix `logq: `.

## Visible test

```bash
node --test
```
