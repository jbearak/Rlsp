import { describe, test, expect, mock, afterEach } from 'bun:test';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ArrowSliceReader } from '../../editors/vscode/src/data-viewer/arrow-reader';

const HERE = dirname(fileURLToPath(import.meta.url));
const FIX = (n: string) =>
    join(HERE, '..', '..', 'editors/vscode/test-fixtures/data-viewer', n);

const openReaders = new Set<ArrowSliceReader>();

async function openReader(
    ...args: Parameters<typeof ArrowSliceReader.open>
): Promise<ArrowSliceReader> {
    const reader = await ArrowSliceReader.open(...args);
    openReaders.add(reader);
    return reader;
}

afterEach(async () => {
    await Promise.all(Array.from(openReaders, reader => reader.close()));
    openReaders.clear();
});

describe('ArrowSliceReader: open + schema (tiny fixture)', () => {
    test('reads schema columns and metadata in declaration order', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        expect(r.schema.columns.map(c => c.name))
            .toEqual(['x', 'y', 's', 'f', 'd', 'ts', 'lbl']);
        expect(r.nrow).toBe(5);
        const y = r.schema.columns.find(c => c.name === 'y')!;
        expect(y.variableLabel).toBe('A floaty column');
        const lbl = r.schema.columns.find(c => c.name === 'lbl')!;
        expect(lbl.valueLabels).toEqual({ '1': 'low', '2': 'mid', '3': 'high' });
        expect(lbl.originalClass).toBe('haven_labelled/vctrs_vctr/double');
    });

    test('factor column ships its dictionary when small', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        const f = r.schema.columns.find(c => c.name === 'f')!;
        expect(f.dictionaryShipped).toBe(true);
        expect(f.dictionary).toEqual(['low', 'med', 'high']);
    });

    test('integer columns are flagged as such', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        const x = r.schema.columns.find(c => c.name === 'x')!;
        expect(x.isInteger).toBe(true);
        const y = r.schema.columns.find(c => c.name === 'y')!;
        expect(y.isInteger).toBe(false);
    });
});

describe('ArrowSliceReader: getRows wire format (tiny fixture)', () => {
    test('encodes float column NA / NaN / ±Inf via sentinels', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        const out = await r.getRows({
            start: 0, end: 5, columns: [1], viewportGeneration: 1,
        });
        expect(out.stale).toBe(false);
        expect(out.rows.length).toBe(5);
        expect(out.rows[0][0]).toBe(1.5);
        expect(out.rows[1][0]).toBeNull();
        expect(out.rows[2][0]).toEqual({ _: 'nan' });
        expect(out.rows[3][0]).toEqual({ _: 'inf' });
        expect(out.rows[4][0]).toEqual({ _: '-inf' });
    });

    test('factor column carries 0-based dictionary index', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        const out = await r.getRows({
            start: 0, end: 5, columns: [3], viewportGeneration: 1,
        });
        expect(out.rows.map(r => r[0])).toEqual([0, 1, 0, 2, 1]);
    });

    test('Date32 cells encode as { _:date, v:YYYY-MM-DD }', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        const out = await r.getRows({
            start: 0, end: 5, columns: [4], viewportGeneration: 1,
        });
        expect(out.rows[0][0]).toEqual({ _: 'date', v: '2024-01-01' });
        expect(out.rows[4][0]).toEqual({ _: 'date', v: '2024-01-05' });
    });

    test('Timestamp cells encode as { _:ts, v:ISO-8601 }', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        const out = await r.getRows({
            start: 0, end: 1, columns: [5], viewportGeneration: 1,
        });
        const cell = out.rows[0][0] as any;
        expect(cell._).toBe('ts');
        expect(cell.v).toBe('2024-01-01T12:00:00Z');
    });

    test('column subset returns only requested columns in order', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        const out = await r.getRows({
            start: 0, end: 1, columns: [2, 0], viewportGeneration: 1,
        });
        expect(out.rows[0]).toHaveLength(2);
        expect(out.rows[0][1]).toBe(1);          // x
        expect(out.rows[0][0]).toBe('a');        // s
    });

    test('clamps end to nrow', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        const out = await r.getRows({
            start: 3, end: 100, columns: [0], viewportGeneration: 1,
        });
        expect(out.rows.length).toBe(2);
        expect(out.rows[0][0]).toBe(4);
    });
});

describe('ArrowSliceReader: multibatch slicing & generation', () => {
    test('getRows(0, 50) loads exactly 1 batch', async () => {
        const r = await openReader(FIX('multibatch.arrow'));
        const loaded: number[] = [];
        r.onBatchLoad = i => loaded.push(i);
        await r.getRows({ start: 0, end: 50, columns: [0, 1], viewportGeneration: 1 });
        expect(loaded).toEqual([0]);
    });

    test('getRows across batch boundary loads exactly 2 batches', async () => {
        const r = await openReader(FIX('multibatch.arrow'));
        const loaded: number[] = [];
        r.onBatchLoad = i => loaded.push(i);
        // chunkSize=100, so batch 0 = rows 0..99, batch 1 = rows 100..199.
        await r.getRows({ start: 95, end: 105, columns: [0], viewportGeneration: 2 });
        expect(loaded).toEqual([0, 1]);
    });

    test('stale viewportGeneration causes early return', async () => {
        const r = await openReader(FIX('multibatch.arrow'));
        r.setLatestViewportGeneration(10);
        const out = await r.getRows({
            start: 0, end: 5, columns: [0], viewportGeneration: 5,
        });
        expect(out.stale).toBe(true);
        expect(out.rows).toEqual([]);
    });

    test('first row in batch 3 has expected values (idempotent decode)', async () => {
        const r = await openReader(FIX('multibatch.arrow'));
        const a = await r.getRows({
            start: 300, end: 301, columns: [0], viewportGeneration: 1,
        });
        const b = await r.getRows({
            start: 300, end: 301, columns: [0], viewportGeneration: 2,
        });
        expect(a.rows[0][0]).toBe(301);
        expect(b.rows[0][0]).toBe(301);
    });
});

describe('ArrowSliceReader: high-cardinality dictionary fallback', () => {
    test('dictionary above injected threshold is not shipped', async () => {
        const r = await openReader(FIX('bigdict.arrow'), {
            dictionaryThreshold: 5,
        });
        const z = r.schema.columns.find(c => c.name === 'zip')!;
        expect(z.dictionaryShipped).toBe(false);
        expect(z.dictionary).toBeUndefined();
    });

    test('getLabels returns labels for the requested indices', async () => {
        const r = await openReader(FIX('bigdict.arrow'), {
            dictionaryThreshold: 5,
        });
        const out = await r.getLabels(0, [0, 1, 5]);
        expect(out[0]).toBe('zip-000');
        expect(out[1]).toBe('zip-001');
        expect(out[5]).toBe('zip-005');
    });
});

describe('ArrowSliceReader: large-file guard', () => {
    // Regression test: open() must not use readFile(), which Node.js caps at
    // ~2 GiB. If readFile is ever reintroduced it will throw here, reproducing
    // the "File size greater than 2 GiB" error seen during smoke testing.
    afterEach(() => { mock.restore(); });

    test('open() succeeds even when readFile is broken', async () => {
        mock.module('node:fs/promises', () => ({
            ...require('node:fs/promises'),
            readFile: () => Promise.reject(new Error('File size (4000394954) is greater than 2 GiB')),
        }));
        const r = await openReader(FIX('multibatch.arrow'));
        expect(r.nrow).toBeGreaterThan(0);
        await r.close();
    });
});

describe('ArrowSliceReader: idempotent close', () => {
    // Calling close() twice is safe — panel.dispose() and panel.replace()
    // can race on the same reader when the user closes the tab during a
    // View() replace. The second call must be a no-op rather than rejecting
    // with EBADF from the underlying FileHandle.
    test('close() is a no-op on the second call', async () => {
        const r = await openReader(FIX('tiny.arrow'));
        await r.close();
        await r.close();
    });
});
