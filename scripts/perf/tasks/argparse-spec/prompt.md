`src/parse-args.js` is a partial implementation of our command-line parser.
`README.md` is the full contract. Please finish the implementation so it matches
the README exactly:

```bash
node --test
```

The visible tests only cover the basics; treat `README.md` as the source of
truth for the edge cases (short-flag clustering, `--name=value`, negative
numbers, missing values, defaults/required, and `splitArgs` quoting/escaping).
Keep the package dependency-free and CommonJS, and keep the existing exports:
`parseArgs`, `splitArgs`, `ArgError`.
