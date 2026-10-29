import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec, expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

expectArgError(() => parseArgs(['--wat'], baseSpec), 'UNKNOWN_OPTION', { option: 'wat' });
expectArgError(() => parseArgs(['--wat=1'], baseSpec), 'UNKNOWN_OPTION', { option: 'wat' });
expectArgError(() => parseArgs(['-z'], baseSpec), 'UNKNOWN_OPTION', { option: 'z' });
expectArgError(() => parseArgs(['-vz'], baseSpec), 'UNKNOWN_OPTION', { option: 'z' });
console.log('ok - unknown long and short options report their names');
