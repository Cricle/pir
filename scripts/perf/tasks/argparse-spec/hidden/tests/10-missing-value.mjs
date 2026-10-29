import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

expectArgError(() => parseArgs(['--name'], baseSpec), 'MISSING_VALUE', { option: 'name' });
expectArgError(() => parseArgs(['--name', '--verbose'], baseSpec), 'MISSING_VALUE', { option: 'name' });
expectArgError(() => parseArgs(['--name', '--'], baseSpec), 'MISSING_VALUE', { option: 'name' });
expectArgError(() => parseArgs(['--tag'], baseSpec), 'MISSING_VALUE', { option: 'tag' });
expectArgError(() => parseArgs(['-n'], baseSpec), 'MISSING_VALUE', { option: 'n' });
expectArgError(() => parseArgs(['-n', '-v'], baseSpec), 'MISSING_VALUE', { option: 'n' });
console.log('ok - missing separate values are detected');
