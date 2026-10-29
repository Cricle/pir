# parse-args

A dependency-free command-line parser. `src/parse-args.js` is a partial
implementation; this document is the complete contract and the source of truth.

## Exports

```js
const { parseArgs, splitArgs, ArgError } = require('./src/parse-args.js');
```

`ArgError` extends `Error` and carries:

* `code` — one of the codes below;
* `option` — the spelling used on the command line when applicable: the long
  name without dashes for `--name`, or the short character for `-x`;
* `value` — the raw string, when applicable.

## `parseArgs(argv, spec)`

`argv` is an array of strings with the program name already removed.

```js
spec = {
  options: {
    verbose: { type: 'boolean', short: 'v' },
    name:    { type: 'string',  short: 'n', required: true },
    count:   { type: 'number',  short: 'c', default: 1 },
    tag:     { type: 'list',    short: 't' },
  },
};
```

`type` is one of `boolean`, `string`, `number`, `list`. `short` is a single
ASCII letter. Return `{ options, positionals }`.

### Long options

* `--name` — boolean options only; sets `true`.
* `--name value` — value options; the next argv element is the value.
* `--name=value` — value options; everything after the first `=` is the value,
  including an empty string (`--name=` gives `''`).
* `--name` on a non-boolean option takes the next argv element using the
  "separate value" rule below.
* `--name=value` on a boolean option throws `UNEXPECTED_VALUE`.
* `--no-name` — boolean options only; sets `false`. `--no-name=value` throws
  `UNEXPECTED_VALUE`. `--no-<x>` where `x` is not a declared boolean is an
  unknown option named `no-<x>`.

### Short options

For an option declared with `short: 'x'`:

* `-x` — boolean sets `true`; a value option takes the next argv element.
* `-x=value` — value after the `=`.
* `-xvalue` — the rest of the token is the value (no separator needed).
* `-x=value` on a boolean throws `UNEXPECTED_VALUE`.
* Short flags may be clustered: `-abc` sets booleans `a` and `b`, and if `c`
  takes a value it consumes the rest of the token (`-abcVALUE` → `c='VALUE'`)
  or the next argv element (`-abc VALUE`). A value option must be the last
  character it owns in the cluster.

### Separate values

A separate (next argv) value may begin with `-` only when it is attached with
`=`/concatenation or when the option type is `number` and the token matches a
negative decimal number (`-5`, `-.5`, `-2.5e3`). Otherwise the token is not
consumed and `MISSING_VALUE` is thrown. A token that looks like a negative
number and does not follow a value option is a positional.

### Positionals and `--`

Options and positionals may be interspersed. `--` terminates option parsing:
all following tokens are positionals verbatim. A bare `-` is a positional.

### Values

* `string` and `list` values are used verbatim.
* `number`: optional sign, decimal digits with optional fraction, optional
  exponent; must be finite. `NaN`, `Infinity`, `0x10`, empty strings and
  surrounding whitespace are invalid → `INVALID_NUMBER` with `.value`.
* `list` options append on every occurrence, in order.

Repeating a scalar option is allowed; the last occurrence wins.

### Defaults and required

Every declared option appears in the returned `options` object.

* Absent boolean → `false` unless `default` is present.
* Absent list → a new empty array, or a shallow copy of `default`.
* Absent string/number → `default` (possibly `undefined`).
* `required: true` throws `MISSING_REQUIRED` when the option is absent and has
  no non-`undefined` `default`.

### Error codes

* `UNKNOWN_OPTION` — `.option` uses the spelling from the command line.
* `MISSING_VALUE` — `.option` as above.
* `UNEXPECTED_VALUE` — boolean given a value.
* `INVALID_NUMBER` — `.value` is the raw offending string.
* `MISSING_REQUIRED` — `.option`.

The first error encountered (left to right) is thrown.

## `splitArgs(line)`

Tokenize one command line into an argv array.

* Spaces and tabs separate tokens; runs collapse; leading/trailing whitespace is
  ignored; whitespace inside quotes is preserved.
* Outside quotes, `\` escapes the next character (including space, quote and
  `\`). A trailing `\` throws `DANGLING_ESCAPE`.
* Single quotes make their contents literal; backslash is not special inside.
* Inside double quotes, `\"` → `"` and `\\` → `\`; any other backslash sequence
  is kept literally.
* Quotes concatenate with adjacent text (`ab"c d"` → `abc d`). Empty quotes
  produce an empty token (`""` → `['']`).
* An unclosed quote throws `UNMATCHED_QUOTE`.
* No variable expansion, globbing or comment handling.

## Visible tests

```bash
node --test
```
