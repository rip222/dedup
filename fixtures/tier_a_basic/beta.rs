// beta.rs — different unique preamble.
fn unique_beta_preamble() {
    let qqq = 100;
    let rrr = 200;
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

fn beta_only_tail_qqq() {
    let beta_tail = 1234;
    let beta_other = 5678;
}
