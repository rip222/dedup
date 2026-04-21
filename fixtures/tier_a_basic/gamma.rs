// gamma.rs — third copy lives further down the file.
fn unique_gamma_preamble() {
    let p1 = 1;
    let p2 = 2;
    let p3 = 3;
    let p4 = 4;
}

// some more unique filler so the duplicate doesn't start on the same line
fn unique_gamma_filler() {
    let z = 42;
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

fn gamma_only_tail_ppp() {
    let gamma_tail = 314;
    let gamma_other = 271;
    let gamma_third = 161;
    let gamma_fourth = 141;
}
