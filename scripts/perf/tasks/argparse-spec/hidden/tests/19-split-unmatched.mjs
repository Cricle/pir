import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { expectArgError } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { splitArgs } = require('../src/parse-args.js');

expectArgError(() => splitArgs("'abc"), 'UNMATCHED_QUOTE');
expectArgError(() => splitArgs('"abc'), 'UNMATCHED_QUOTE');
expectArgError(() => splitArgs(`a 'b c`), 'UNMATCHED_QUOTE');
expectArgError(() => splitArgs('a "b c'), 'UNMATCHED_QUOTE');
assert.deepEqual(splitArgs('"a\'b"'), ["a'b"]);
console.log('ok - unclosed quotes throw UNMATCHED_QUOTE');
