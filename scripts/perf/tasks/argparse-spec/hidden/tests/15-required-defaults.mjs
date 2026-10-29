import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const spec = {
  options: {
    must: { type: 'string', required: true },
    optional: { type: 'string' },
    flag: { type: 'boolean' },
    items: { type: 'list', default: ['a'] },
    num: { type: 'number', default: 0 },
  },
};

expectArgError(() => parseArgs([], spec), 'MISSING_REQUIRED', { option: 'must' });
expectArgError(() => parseArgs(['--optional', 'x'], spec), 'MISSING_REQUIRED', { option: 'must' });

const parsed = parseArgs(['--must', 'yes'], spec);
assert.equal(parsed.options.must, 'yes');
assert.equal(parsed.options.optional, undefined);
assert.equal(parsed.options.flag, false);
assert.deepEqual(parsed.options.items, ['a']);
assert.equal(parsed.options.num, 0);

const defaultedRequired = { options: { must: { type: 'string', required: true, default: 'd' } } };
assert.equal(parseArgs([], defaultedRequired).options.must, 'd');
console.log('ok - required checks and per-type defaults');
