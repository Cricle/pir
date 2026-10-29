`lib/textstats.js` has grown too large. Split it into modules without changing
its public behavior or its public API.

Required layout:

```text
lib/textstats.js            thin compatibility shim
lib/textstats/tokenize.js   exports { tokenize, splitSentences }
lib/textstats/stats.js      exports { DEFAULT_STOPWORDS, wordFrequency, topWords, summarize }
lib/textstats/format.js     exports { formatTable, formatSummary }
lib/textstats/index.js      exports all 8 names by composing the three modules
```

Hard constraints:

- `require('./lib/textstats.js')` keeps working and exposes exactly the same
  8 exports as before; it must re-export `./textstats/index.js`, be at most
  10 lines, and contain no function or class definitions.
- `lib/textstats/index.js` exports exactly those 8 names and re-exports the
  same function objects as the three modules. It is at most 60 lines.
- `tokenize.js`, `stats.js` and `format.js` are each at most 160 lines.
- Dependency direction: `tokenize.js` and `format.js` are leaves; `stats.js`
  may require only `./tokenize.js`; `index.js` requires the three modules.
- Behavior, return shapes, sorting and thrown error types must not change.

Verify with:

```bash
node --test
```
