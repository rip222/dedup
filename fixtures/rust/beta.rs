// beta.rs — sink side of the Type-1 and Type-2 duplicates.

// Type-1 duplicate: byte-identical to alpha.rs::type1_identical.
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

// Type-2 duplicate: same structure as alpha.rs::type2_original, local
// bindings and parameter names renamed. Tier B must alpha-rename
// these to canonical aliases and bucket them together.
fn type2_renamed(entries: &[Item]) -> Summary {
    let mut acc = 0;
    let mut n = 0;
    for entry in entries {
        acc += entry.weight;
        n += 1;
    }
    let avg = if n > 0 { acc / n } else { 0 };
    Summary { total: acc, seen: n, avg: avg }
}

// Unique function — must stay out of every match group.
fn unique_beta() -> String {
    let greeting = "hi";
    greeting.to_string()
}
