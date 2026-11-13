#!/usr/bin/env python3
"""Create an isolated, deterministic UI/search fixture. Never opens the live DB."""

import argparse
import json
from pathlib import Path
import sqlite3
import time


def create(root: Path, entries: int):
    root = root.resolve()
    data = root / 'data'
    files = root / 'files'
    if (data / 'index.db').exists():
        raise SystemExit(f'Refusing to overwrite an existing index: {data}')
    data.mkdir(parents=True, exist_ok=True)
    files.mkdir(exist_ok=True)
    (root / '.everything-ui-fixture').write_text('Everything GPUI validation fixture\n')
    real_files = {
        'Alpha report.txt': b'Alpha report\n',
        'alpha-summary.TXT': b'Case-insensitive search\n',
        'report final 2026.pdf': b'%PDF-1.1\n1 0 obj<</Type/Catalog>>endobj\n%%EOF',
        '한글 보고서.txt': '한국어 검색 테스트\n'.encode(),
        'space name.txt': b'A filename containing spaces\n',
        'unicode-École.md': b'Unicode filename\n',
        'notes.md': b'# Fixture notes\n',
        'gpui-tc-open.txt': b'Only this fixture is opened during the UI test.\n',
        'gpui-tc-rename.txt': b'Rename fixture\n',
        'gpui-tc-trash.txt': b'Trash fixture\n',
        'gpui-tc-collision.txt': b'Existing target\n',
        'folder-demo/child.txt': b'Child of selected folder\n',
        'folder-demo-other/keep.txt': b'Sibling prefix must survive\n',
        'Demo.app/Contents/Info.plist': b'<?xml version="1.0"?><plist version="1.0"><dict></dict></plist>',
    }
    for relative, content in real_files.items():
        path = files / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    conn = sqlite3.connect(data / 'index.db')
    conn.executescript('''
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=OFF;
        PRAGMA user_version=7;
        CREATE TABLE entries (
            id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL, dir TEXT NOT NULL, is_dir INTEGER NOT NULL,
            ext TEXT, mtime INTEGER, size INTEGER, indexed_at INTEGER NOT NULL,
            run_id INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
    ''')
    epoch = 1794500000
    rows = []
    for path in sorted(files.rglob('*')):
        stat = path.stat()
        is_dir = path.is_dir()
        rows.append((str(path), path.name, str(path.parent), int(is_dir),
                     None if is_dir else path.suffix[1:].lower(),
                     epoch, None if is_dir else stat.st_size, epoch, 1))
    insert = 'INSERT INTO entries(path,name,dir,is_dir,ext,mtime,size,indexed_at,run_id) VALUES (?,?,?,?,?,?,?,?,?)'
    conn.executemany(insert, rows)
    extensions = ['txt', 'md', 'rs', 'json', 'pdf', 'png', 'csv', 'log']
    for start in range(0, entries, 10000):
        batch = []
        for i in range(start, min(start + 10000, entries)):
            extension = extensions[i % len(extensions)]
            name = f'report_{i:07d}.{extension}'
            directory = files / 'virtual' / f'project-{i % 100:03d}'
            batch.append((str(directory / name), name, str(directory), 0, extension,
                          epoch - i % 100000, 128 + (i * 7919) % 20000000, epoch, 1))
        conn.executemany(insert, batch)
    conn.executescript('''
        CREATE INDEX idx_entries_name_nocase ON entries(name COLLATE NOCASE);
        CREATE INDEX idx_entries_dir_ext_name_nocase ON entries(dir,ext,name COLLATE NOCASE);
        CREATE INDEX idx_entries_ext_name ON entries(ext,name COLLATE NOCASE);
        CREATE INDEX idx_entries_mtime ON entries(mtime);
        CREATE INDEX idx_entries_indexed_at ON entries(indexed_at);
        CREATE VIRTUAL TABLE entries_fts USING fts5(name,content='entries',content_rowid='id',tokenize='trigram');
        INSERT INTO entries_fts(entries_fts) VALUES('rebuild');
        CREATE TRIGGER entries_ai AFTER INSERT ON entries BEGIN
          INSERT INTO entries_fts(rowid,name) VALUES(new.id,new.name);
        END;
        CREATE TRIGGER entries_ad AFTER DELETE ON entries BEGIN
          INSERT INTO entries_fts(entries_fts,rowid,name) VALUES('delete',old.id,old.name);
        END;
        CREATE TRIGGER entries_au AFTER UPDATE OF name ON entries BEGIN
          INSERT INTO entries_fts(entries_fts,rowid,name) VALUES('delete',old.id,old.name);
          INSERT INTO entries_fts(rowid,name) VALUES(new.id,new.name);
        END;
        ANALYZE;
    ''')
    total = entries + len(rows)
    conn.executemany('INSERT INTO meta VALUES(?,?)', [
        ('fts_dirty', '0'), ('index_complete', '1'), ('last_run_id', '1'),
        ('cached_entries_count', str(total)), ('cached_last_updated', str(epoch)),
    ])
    conn.commit()
    conn.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    conn.close()
    manifest = {'root': str(root), 'data': str(data), 'files': str(files),
                'entries': total, 'virtualEntries': entries, 'realFiles': list(real_files),
                'schemaVersion': 7, 'createdAt': time.time()}
    (root / 'fixture.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2))
    print(json.dumps(manifest, ensure_ascii=False))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('--entries', type=int, default=500000)
    args = parser.parse_args()
    if not 0 <= args.entries <= 2000000:
        parser.error('--entries must be between 0 and 2000000')
    create(args.directory, args.entries)
