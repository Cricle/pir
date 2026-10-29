# date-range

```js
const { enumerateDates } = require('./src/date-range.js');
```

## Contract

`enumerateDates(start, end)` returns every calendar date from `start`
inclusive to `end` exclusive, as `YYYY-MM-DD` strings.

* Inputs must be strings matching exactly `YYYY-MM-DD` (four-digit year, two
  digits for month and day, zero padded). Years are `0001`–`9999`.
* The date must exist on the Gregorian calendar (for example `2024-02-29` is
  valid, `2023-02-29`, `2024-04-31` and `2024-13-01` are not).
* Any non-string, malformed, or impossible date throws `RangeError`.
* If `start === end`, the result is `[]` (half-open).
* If `end < start`, `RangeError` is thrown.
* The arithmetic is calendar arithmetic in UTC: the result must be identical
  in every process timezone and across DST transitions.
* Results are zero padded: `['2024-02-28', '2024-02-29']`.

## Tests

```bash
node --test
```
