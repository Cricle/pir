# textstats

Dependency-free text statistics. Public API:

```js
const {
  DEFAULT_STOPWORDS, tokenize, splitSentences, wordFrequency,
  topWords, summarize, formatTable, formatSummary,
} = require('./lib/textstats.js');
```

See the JSDoc in `lib/textstats.js` for exact behavior. The test suite in
`lib/textstats.test.js` is the current compatibility check:

```bash
node --test
```
