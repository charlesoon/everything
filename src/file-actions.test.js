import assert from 'node:assert/strict';
import test from 'node:test';
import { excludeTrashedEntries } from './file-actions.js';
import { shouldApplyPreserveResults } from './search-utils.js';

const entries = (paths) => paths.map((path) => ({ path }));

test('deleted directories disappear with their children while prefix siblings remain', () => {
  const rows = entries(['/work/report', '/work/report/a.txt', '/work/report-old/a.txt', '/work/keep.txt']);
  assert.deepEqual(
    excludeTrashedEntries(rows, new Set(['/work/report']), 'macos'),
    entries(['/work/report-old/a.txt', '/work/keep.txt'])
  );
});

test('partial deletion removes successful paths and keeps failed selections', () => {
  const rows = entries(['/work/locked.txt', '/work/ok.txt', '/work/other.txt']);
  assert.deepEqual(
    excludeTrashedEntries(rows, ['/work/ok.txt', '/work/other.txt'], 'macos'),
    entries(['/work/locked.txt'])
  );
});

test('macOS paths preserve literal backslashes and case', () => {
  const rows = entries(['/work/name\\', '/work/name', '/work/NAME\\']);
  assert.deepEqual(
    excludeTrashedEntries(rows, ['/work/name\\'], 'macos'),
    entries(['/work/name', '/work/NAME\\'])
  );
});

test('Windows paths match separators and case without hiding prefix siblings', () => {
  const rows = entries(['C:\\Work\\Folder\\a.txt', 'C:/Work/Folder/b.txt', 'C:\\Work\\Folder-old\\a.txt']);
  assert.deepEqual(
    excludeTrashedEntries(rows, ['c:\\work\\folder'], 'windows'),
    entries(['C:\\Work\\Folder-old\\a.txt'])
  );
});

test('refresh applies a list reduced by deletion even when it cannot fill the viewport', () => {
  assert.equal(shouldApplyPreserveResults(0, 0), true);
  assert.equal(shouldApplyPreserveResults(2, 0), true);
  assert.equal(shouldApplyPreserveResults(21, 20), true);
  assert.equal(shouldApplyPreserveResults(20, 20), false);
});
