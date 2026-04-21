// alpha.rs — source side of the Type-1 and Type-2 duplicates.

// Type-1 duplicate: identical to `type1_identical` in beta.rs.
fn type1_identical(rows: &[Row]) -> Total {
    let mut sum = 0;
    let mut count = 0;
    for r in rows {
        sum += r.value;
        count += 1;
    }
    let mean = if count > 0 { sum / count } else { 0 };
    Total { sum, count, mean }
}

// Type-2 duplicate: same shape as `type2_renamed` in beta.rs,
// differing only in local variable names.
fn type2_original(items: &[Item]) -> Summary {
    let mut tot = 0;
    let mut cnt = 0;
    for item in items {
        tot += item.weight;
        cnt += 1;
    }
    let mean = if cnt > 0 { tot / cnt } else { 0 };
    Summary { total: tot, seen: cnt, avg: mean }
}

// Unique function that must NOT be grouped. It is short (small number
// of tokens) relative to the duplicates, and semantically distinct.
fn unique_alpha() -> i32 {
    let base = 7;
    base * 2 + 1
}
