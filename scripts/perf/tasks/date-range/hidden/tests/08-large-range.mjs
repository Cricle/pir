import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { enumerateDates } = require('../src/date-range.js');

const dates = enumerateDates('2024-01-01', '2034-01-01');
assert.equal(dates.length, 3653);
assert.equal(dates[0], '2024-01-01');
assert.equal(dates[dates.length - 1], '2033-12-31');
assert.ok(dates.includes('2024-02-29'));
assert.ok(dates.includes('2028-02-29'));
assert.ok(dates.includes('2032-02-29'));
assert.ok(dates.every((date) => /^\d{4}-\d{2}-\d{2}$/.test(date)));
assert.equal(new Set(dates).size, dates.length);
console.log('ok - a ten-year range is complete, ordered and unique');
