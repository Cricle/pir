import assert from 'node:assert/strict';
import { jsonLines, recordsFile, run } from './_util.mjs';

const file = recordsFile([
  { ts: 't1', level: 'info', msg: 'hello', user: { id: 7, name: 'Ada' }, extra: true },
  { ts: 't2', level: 'error', msg: 'oops', user: { id: 8 } },
]);
try {
  const projected = run(['--fields', 'ts,user.id,missing', file.path]);
  assert.equal(projected.status, 0, projected.stderr);
  const lines = projected.stdout.trim().split('\n');
  assert.deepEqual(lines, [
    '{"ts":"t1","user.id":7,"missing":null}',
    '{"ts":"t2","user.id":8,"missing":null}',
  ]);

  const names = jsonLines(run(['--fields', 'user.name', file.path]).stdout);
  assert.deepEqual(names, [{ 'user.name': 'Ada' }, { 'user.name': null }]);

  const whole = jsonLines(run([file.path]).stdout);
  assert.deepEqual(whole[0].user, { id: 7, name: 'Ada' });
  assert.equal(whole[0].extra, true);

  const equals = jsonLines(run(['--fields=ts,msg', file.path]).stdout);
  assert.deepEqual(equals[0], { ts: 't1', msg: 'hello' });

  const bad = run(['--fields', 'ts,,msg', file.path]);
  assert.equal(bad.status, 2);
} finally {
  file.cleanup();
}
console.log('ok - field projection uses flat dot-path keys and null for missing values');
