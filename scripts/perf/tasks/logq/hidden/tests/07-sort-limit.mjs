import assert from 'node:assert/strict';
import { jsonLines, recordsFile, run } from './_util.mjs';

const file = recordsFile([
  { id: 'a', latency_ms: 30, tag: 'b' },
  { id: 'b', latency_ms: 10, tag: 'a' },
  { id: 'c', latency_ms: 20, tag: 'c' },
  { id: 'd', tag: 'a' },
]);
const ids = (args) => {
  const result = run(args);
  assert.equal(result.status, 0, result.stderr);
  return jsonLines(result.stdout).map((record) => record.id);
};
try {
  assert.deepEqual(ids(['--sort', 'latency_ms', file.path]), ['b', 'c', 'a', 'd']);
  assert.deepEqual(ids(['--sort', '-latency_ms', file.path]), ['a', 'c', 'b', 'd']);
  assert.deepEqual(ids(['--sort', 'latency_ms', '--limit', '2', file.path]), ['b', 'c']);
  assert.deepEqual(ids([file.path]), ['a', 'b', 'c', 'd']);
  assert.deepEqual(ids(['--sort', 'tag', file.path]), ['b', 'd', 'a', 'c']);

  for (const limit of ['0', '-1', 'abc']) {
    assert.equal(run(['--limit', limit, file.path]).status, 2, `limit ${limit} must fail`);
  }
} finally {
  file.cleanup();
}
console.log('ok - stable sorting, missing-last and positive limit');
