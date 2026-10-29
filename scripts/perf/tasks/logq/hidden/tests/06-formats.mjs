import assert from 'node:assert/strict';
import { recordsFile, run, tempFile } from './_util.mjs';

const file = recordsFile([
  { ts: 't1', msg: 'plain', tag: 'a,b' },
  { ts: 't2', msg: 'say "hi"', tag: 'line\nbreak' },
  { ts: 't3', msg: null, tag: 'x' },
]);
try {
  const csv = run(['--fields', 'ts,msg,tag', '--format', 'csv', file.path]);
  assert.equal(csv.status, 0, csv.stderr);
  assert.equal(
    csv.stdout,
    'ts,msg,tag\n' +
      't1,plain,"a,b"\n' +
      't2,"say ""hi""","line\nbreak"\n' +
      't3,,x\n',
  );

  const json = run(['--fields', 'ts,msg', file.path]);
  assert.equal(json.status, 0, json.stderr);
  assert.equal(
    json.stdout,
    '{"ts":"t1","msg":"plain"}\n{"ts":"t2","msg":"say \\"hi\\""}\n{"ts":"t3","msg":null}\n',
  );

  const noFields = run(['--format', 'csv', file.path]);
  assert.equal(noFields.status, 2);

  const empty = tempFile('# nothing here\n\n', 'empty.log');
  try {
    const header = run(['--fields', 'a,b', '--format', 'csv', empty.path]);
    assert.equal(header.status, 0, header.stderr);
    assert.equal(header.stdout, 'a,b\n');
  } finally {
    empty.cleanup();
  }
} finally {
  file.cleanup();
}
console.log('ok - compact JSON and RFC4180 CSV output');
