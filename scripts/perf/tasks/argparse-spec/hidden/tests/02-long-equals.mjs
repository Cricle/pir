import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { baseSpec } from './_spec.mjs';

const require = createRequire(import.meta.url);
const { parseArgs } = require('../src/parse-args.js');

const parsed = parseArgs(['--name=Alice', '--tag=a b', '--count=-2.5e1', '--tag=', '--name='], baseSpec);
assert.equal(parsed.options.name, '');
assert.equal(parsed.options.count, -25);
assert.deepEqual(parsed.options.tag, ['a b', '']);
assert.deepEqual(parsed.positionals, []);
console.log('ok - --name=value keeps empty values and spaces');
