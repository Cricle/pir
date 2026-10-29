import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

expectArgError(() => parseArgs(['--verbose=true'], baseSpec), 'UNEXPECTED_VALUE', { option: 'verbose' });
expectArgError(() => parseArgs(['-v=1'], baseSpec), 'UNEXPECTED_VALUE', { option: 'v' });
expectArgError(() => parseArgs(['--no-quiet=1'], baseSpec), 'UNEXPECTED_VALUE', { option: 'quiet' });
assert.equal(parseArgs(['--verbose'], baseSpec).options.verbose, true);
console.log('ok - boolean options reject attached values');
