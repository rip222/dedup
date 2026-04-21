// alpha.rs — unique preamble so rolling-hash windows don't leak in.
fn unique_alpha_preamble() {
    let zzz = 0;
    let yyy = 1;
    let xxx = 2;
}

// --- shared duplicated block starts here ---
fn compute_totals(rows: &[Row]) -> Total {
    let mut sum = 0;
    let mut count = 0;
    for r in rows {
        sum += r.value;
        count += 1;
    }
    let mean = if count > 0 { sum / count } else { 0 };
    let max = rows.iter().map(|r| r.value).max().unwrap_or(0);
    let min = rows.iter().map(|r| r.value).min().unwrap_or(0);
    Total { sum, count, mean, max, min }
}
// --- shared duplicated block ends here ---

fn alpha_only_tail_zzz() {
    let alpha_tail = 999;
    let alpha_other = 888;
    let alpha_third = 777;
}
