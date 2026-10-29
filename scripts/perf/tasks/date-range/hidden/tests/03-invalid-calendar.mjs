import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { enumerateDates } = require('../src/date-range.js');

for (const bad of [
  '2024-02-30',
  '2023-02-29',
  '2024-04-31',
  '2024-13-01',
  '2024-00-10',
  '2024-01-00',
  '2024-01-32',
  '0000-01-01',
  '1900-02-29',
]) {
  assert.throws(
    () => enumerateDates(bad, '2024-03-02'),
    RangeError,
    `${bad} must be rejected`,
  );
}

for (const bad of [42, null, undefined, true, {}, []]) {
  assert.throws(() => enumerateDates(bad, '2024-03-02'), RangeError);
  assert.throws(() => enumerateDates('2024-03-01', bad), RangeError);
}
console.log('ok - impossible calendar dates and non-strings throw RangeError');
