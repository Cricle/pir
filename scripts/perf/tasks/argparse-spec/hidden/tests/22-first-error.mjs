import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

// The leftmost error wins, even if a later option would also be invalid.
expectArgError(
  () => parseArgs(['--count=bad', '--wat'], baseSpec),
  'INVALID_NUMBER',
  { option: 'count', value: 'bad' },
);
expectArgError(
  () => parseArgs(['--wat', '--count=bad'], baseSpec),
  'UNKNOWN_OPTION',
  { option: 'wat' },
);
expectArgError(
  () => parseArgs(['--verbose', '--tag', '--count=1'], baseSpec),
  'MISSING_VALUE',
  { option: 'tag' },
);
console.log('ok - the first error in argv order is reported');
