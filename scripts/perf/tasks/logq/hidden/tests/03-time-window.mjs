import assert from 'node:assert/strict';
import { jsonLines, recordsFile, run } from './_util.mjs';

const file = recordsFile([
  { id: 'A', ts: '2024-01-01T00:00:00Z' },
  { id: 'B', ts: '2024-01-01T12:00:00+05:30' },
  { id: 'C', ts: '2024-01-02T00:00:00Z' },
  { id: 'D' },
  { id: 'E', ts: 'not-a-date' },
]);
const ids = (args) => {
  const result = run(args);
  assert.equal(result.status, 0, result.stderr);
  return jsonLines(result.stdout).map((record) => record.id);
};
try {
  assert.deepEqual(
    ids(['--since', '2024-01-01T06:00:00Z', '--until', '2024-01-02T00:00:00Z', file.path]),
    ['B'],
  );
  assert.deepEqual(ids(['--since', '2024-01-01T00:00:00Z', file.path]), ['A', 'B', 'C']);
  assert.deepEqual(ids(['--until', '2024-01-01T06:30:00Z', file.path]), ['A']);

  const bad = run(['--since', 'nope', file.path]);
  assert.equal(bad.status, 2);
} finally {
  file.cleanup();
}
console.log('ok - half-open time windows honor offsets and skip unparseable timestamps');
