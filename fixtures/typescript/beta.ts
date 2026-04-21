// beta.ts — dedupe partner for alpha.ts.

import { Row, Total } from "./types";
import * as util from "./util";

// Type-1 duplicate of alpha.ts::type1Identical — same everything.
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

// Type-2 duplicate of alpha.ts::type2Original — locals + params
// renamed, same control flow, same property names, same literal shape.
const type2Renamed = (entries: Row[]): Total => {
    let total = 0;
    let seen = 0;
    for (const entry of entries) {
        total += entry.value;
        seen += 1;
    }
    const average = seen > 0 ? total / seen : 0;
    return { sum: total, count: seen, mean: average };
};

// Unique function with a string literal that must NOT collapse into
// the number-only uniqueAlpha in alpha.ts.
function uniqueBeta(): string {
    const greeting = "hello";
    return greeting.toUpperCase();
}
