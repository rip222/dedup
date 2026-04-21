// alpha.ts — source side of the TS Type-1 and Type-2 duplicate pairs.

import { Row, Total } from "./types";
import * as util from "./util";

// Type-1 duplicate: identical to `type1Identical` in beta.ts.
function type1Identical(rows: Row[]): Total {
    let sum = 0;
    let count = 0;
    for (const r of rows) {
        sum += r.value;
        count += 1;
    }
    const mean = count > 0 ? sum / count : 0;
    return { sum, count, mean };
}

// Type-2 duplicate: same shape as `type2Renamed` in beta.ts, differs
// only in local binding and parameter names.
const type2Original = (items: Row[]): Total => {
    let tot = 0;
    let cnt = 0;
    for (const item of items) {
        tot += item.value;
        cnt += 1;
    }
    const avg = cnt > 0 ? tot / cnt : 0;
    return { sum: tot, count: cnt, mean: avg };
};

// Unique arrow — must NOT bucket with the duplicates.
const uniqueAlpha = (): number => {
    const base = 7;
    return base * 2 + 1;
};

export class Aggregator {
    constructor(private readonly factor: number) {}

    // Method is itself a syntactic unit; body uses `this` which stays.
    scale(values: number[]): number {
        let acc = 0;
        for (const v of values) {
            acc += v * this.factor;
        }
        return acc;
    }
}

export { type1Identical, type2Original, uniqueAlpha };
