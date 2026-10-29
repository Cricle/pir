import assert from 'node:assert/strict';
import { join } from 'node:path';
import { run, tempFile } from './_util.mjs';

const a = tempFile('{"id":1}\n\n# comment\n{"id":2}\n', 'a.log');
const b = tempFile('{"id":3}\n', 'b.log');
try {
  const result = run([a.path, b.path]);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(
    result.stdout.trim().split('\n').filter(Boolean).map((line) => JSON.parse(line).id),
    [1, 2, 3],
  );

  const stdin = run([], { input: '\n{"id":4}\n  # ignored\n{"id":5}\n' });
  assert.equal(stdin.status, 0, stdin.stderr);
  assert.deepEqual(
    stdin.stdout.trim().split('\n').filter(Boolean).map((line) => JSON.parse(line).id),
    [4, 5],
  );

  const dash = run(['-'], { input: '{"id":6}\n' });
  assert.equal(dash.status, 0, dash.stderr);
  assert.equal(JSON.parse(dash.stdout.trim()).id, 6);
} finally {
  a.cleanup();
  b.cleanup();
}

const malformed = tempFile('{"id":1}\nnot json\n', 'bad.log');
try {
  const result = run([malformed.path]);
  assert.equal(result.status, 2);
  assert.match(result.stderr, /line 2: invalid json/);
} finally {
  malformed.cleanup();
}

const array = tempFile('[1,2]\n', 'array.log');
try {
  const result = run([array.path]);
  assert.equal(result.status, 2);
  assert.match(result.stderr, /line 1: invalid json/);
} finally {
  array.cleanup();
}

const first = tempFile('{"id":1}\n', 'first.log');
const second = tempFile('{"id":2}\nnot json\n', 'second.log');
try {
  const result = run([first.path, second.path]);
  assert.equal(result.status, 2);
  assert.match(result.stderr, /line 3: invalid json/);
} finally {
  first.cleanup();
  second.cleanup();
}
console.log('ok - input concatenation, global line numbers, stdin and invalid JSON');
