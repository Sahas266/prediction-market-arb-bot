//! Replay evaluator for the arb monitor.
//!
//! Reads logged `books_log` and `opportunities` from SQLite and produces
//! a statistical report:
//!   - Opportunity persistence histogram (how many cycles each opp lasted)
//!   - Net edge distribution (median, p25, p75, p95)
//!   - False positive rate estimate (single-tick vs persistent opportunities)
//!   - Recommended threshold calibration
//!   - Per-pair breakdown
//!
//! Usage: cargo run --release --bin replay_eval
//! Optional: cargo run --release --bin replay_eval -- --hours 48

use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

// ── CLI args (minimal, no dep needed) ───────────────────────────────────────

struct Args {
    /// Only consider data from the last N hours (0 = all data)
    hours: u32,
    /// Minimum net edge (as fraction) to count an opportunity as real
    min_edge: f64,
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    let mut hours = 0u32;
    let mut min_edge = 0.01;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--hours" | "-h" => {
                if let Some(v) = args.get(i + 1) {
                    hours = v.parse().unwrap_or(0);
                    i += 1;
                }
            }
            "--min-edge" | "-e" => {
                if let Some(v) = args.get(i + 1) {
                    min_edge = v.parse().unwrap_or(0.01);
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    Args { hours, min_edge }
}

fn project_root() -> PathBuf {
    let mut dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if dir.join("config.yaml").exists() {
            return dir;
        }
        if !dir.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
}

// ── Statistics helpers ───────────────────────────────────────────────────────

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (p / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn histogram(values: &[u32], buckets: &[u32]) -> Vec<(String, usize)> {
    let mut counts = vec![0usize; buckets.len() + 1];
    for &v in values {
        let bucket = buckets.partition_point(|&b| v > b);
        counts[bucket] += 1;
    }
    let mut result = Vec::new();
    for (i, &b) in buckets.iter().enumerate() {
        let label = if i == 0 {
            format!("1 cycle")
        } else {
            format!("{}-{}", buckets[i - 1] + 1, b)
        };
        result.push((label, counts[i]));
    }
    result.push((format!(">{}", buckets.last().unwrap_or(&0)), counts[buckets.len()]));
    result
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args = parse_args();
    let root = project_root();
    let db_path = root.join("data").join("arb.db");

    if !db_path.exists() {
        eprintln!("No database found at {}.", db_path.display());
        eprintln!("Run `arb_monitor` first to collect data.");
        std::process::exit(1);
    }

    let conn = Connection::open(&db_path)?;

    let time_filter = if args.hours > 0 {
        format!("AND ts_received >= datetime('now', '-{} hours')", args.hours)
    } else {
        String::new()
    };

    println!("=== Replay Evaluator ===");
    println!("Database: {}", db_path.display());
    if args.hours > 0 {
        println!("Window:   last {} hours", args.hours);
    } else {
        println!("Window:   all data");
    }
    println!("Min edge: {:.1}%", args.min_edge * 100.0);
    println!();

    // ── Section 1: Books log overview ────────────────────────────────────────

    println!("── Books Log ──────────────────────────────────────────────────");

    let books_count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM books_log WHERE 1=1 {}", time_filter),
        [],
        |r| r.get(0),
    )?;

    let earliest: Option<String> = conn
        .query_row(
            "SELECT MIN(ts_received) FROM books_log",
            [],
            |r| r.get(0),
        )
        .ok();

    let latest: Option<String> = conn
        .query_row(
            "SELECT MAX(ts_received) FROM books_log",
            [],
            |r| r.get(0),
        )
        .ok();

    println!("  Total book snapshots: {}", books_count);
    println!(
        "  Date range: {} → {}",
        earliest.as_deref().unwrap_or("n/a"),
        latest.as_deref().unwrap_or("n/a")
    );

    // Books per venue
    let mut stmt = conn.prepare(&format!(
        "SELECT venue, COUNT(*) FROM books_log WHERE 1=1 {} GROUP BY venue",
        time_filter
    ))?;
    let venue_counts: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    for (venue, count) in &venue_counts {
        println!("    {}: {} snapshots", venue, count);
    }

    // Books per canonical pair
    let mut stmt = conn.prepare(&format!(
        "SELECT canonical_id, COUNT(*) as n FROM books_log WHERE canonical_id IS NOT NULL {} \
         GROUP BY canonical_id ORDER BY n DESC",
        time_filter
    ))?;
    let pair_counts: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    println!("\n  Snapshots per canonical pair:");
    for (id, count) in &pair_counts {
        println!("    {:<40} {}", id, count);
    }
    println!();

    // ── Section 2: Opportunity overview ──────────────────────────────────────

    println!("── Opportunities ──────────────────────────────────────────────");

    let opp_count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM opportunities WHERE CAST(net_edge AS REAL) >= {} {}",
            args.min_edge, time_filter.replace("ts_received", "detected_at")
        ),
        [],
        |r| r.get(0),
    )?;

    let total_opp_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM opportunities",
        [],
        |r| r.get(0),
    )?;

    println!("  Total detected:   {}", total_opp_count);
    println!(
        "  Above {:.1}% edge: {}",
        args.min_edge * 100.0,
        opp_count
    );

    if total_opp_count == 0 {
        println!("\n  No opportunities logged yet. Run arb_monitor to collect data.");
        return Ok(());
    }

    // Net edge distribution
    let mut stmt = conn.prepare(
        "SELECT CAST(net_edge AS REAL) FROM opportunities ORDER BY CAST(net_edge AS REAL)",
    )?;
    let mut edges: Vec<f64> = stmt
        .query_map([], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .filter(|e: &f64| e.is_finite())
        .collect();
    edges.sort_by(|a, b| a.partial_cmp(b).unwrap());

    if !edges.is_empty() {
        println!("\n  Net edge distribution:");
        println!("    min:  {:.4} ({:.2}%)", edges[0], edges[0] * 100.0);
        println!("    p25:  {:.4} ({:.2}%)", percentile(&edges, 25.0), percentile(&edges, 25.0) * 100.0);
        println!("    median: {:.4} ({:.2}%)", percentile(&edges, 50.0), percentile(&edges, 50.0) * 100.0);
        println!("    mean: {:.4} ({:.2}%)", mean(&edges), mean(&edges) * 100.0);
        println!("    p75:  {:.4} ({:.2}%)", percentile(&edges, 75.0), percentile(&edges, 75.0) * 100.0);
        println!("    p95:  {:.4} ({:.2}%)", percentile(&edges, 95.0), percentile(&edges, 95.0) * 100.0);
        println!("    max:  {:.4} ({:.2}%)", edges[edges.len()-1], edges[edges.len()-1] * 100.0);
    }

    // ── Section 3: Persistence analysis ──────────────────────────────────────

    println!("\n── Opportunity Persistence ────────────────────────────────────");
    println!("  (How many consecutive cycles each canonical_id appeared)");
    println!("  Note: persistence is approximated from detected_at timestamps.\n");

    // Group opportunities by canonical_id, count how many detection events there are
    // and the time span (max - min detected_at per canonical_id)
    let mut stmt = conn.prepare(
        "SELECT canonical_id, COUNT(*) as detections, \
         MIN(detected_at) as first_seen, MAX(detected_at) as last_seen \
         FROM opportunities \
         GROUP BY canonical_id \
         ORDER BY detections DESC",
    )?;

    struct OppGroup {
        detections: u32,
    }

    let groups: Vec<OppGroup> = stmt
        .query_map([], |r| Ok(OppGroup { detections: r.get(1)? }))?
        .filter_map(|r| r.ok())
        .collect();

    // Persistence histogram
    let detection_counts: Vec<u32> = groups.iter().map(|g| g.detections).collect();
    let buckets = [1u32, 2, 3, 5, 10, 20, 50];
    let hist = histogram(&detection_counts, &buckets);

    println!("  Detection count distribution:");
    for (label, count) in &hist {
        let bar_width = (*count as f64 / groups.len() as f64 * 40.0) as usize;
        println!("    {:>8} cycles: {:>4}  {}", label, count, "█".repeat(bar_width));
    }

    let single_tick = detection_counts.iter().filter(|&&d| d == 1).count();
    let persistent = detection_counts.iter().filter(|&&d| d >= 3).count();
    let false_positive_pct = if !groups.is_empty() {
        single_tick as f64 / groups.len() as f64 * 100.0
    } else {
        0.0
    };

    println!(
        "\n  Single-tick (likely noise): {} / {} ({:.1}%)",
        single_tick, groups.len(), false_positive_pct
    );
    println!(
        "  Persistent (≥3 cycles):     {} / {} ({:.1}%)",
        persistent, groups.len(),
        persistent as f64 / groups.len().max(1) as f64 * 100.0
    );

    // ── Section 4: Per-pair breakdown ────────────────────────────────────────

    println!("\n── Per-Pair Breakdown ─────────────────────────────────────────");
    println!(
        "  {:<40} {:>8} {:>8} {:>8} {:>8}",
        "Canonical ID", "Detects", "MaxEdge%", "MedEdge%", "Persist"
    );
    println!("  {}", "-".repeat(80));

    let mut stmt = conn.prepare(
        "SELECT canonical_id, COUNT(*) as n, \
         MAX(CAST(net_edge AS REAL)) as max_edge, \
         AVG(CAST(net_edge AS REAL)) as avg_edge \
         FROM opportunities \
         GROUP BY canonical_id \
         ORDER BY n DESC",
    )?;

    struct PairStat {
        canonical_id: String,
        n: u32,
        max_edge: f64,
        avg_edge: f64,
    }

    let pair_stats: Vec<PairStat> = stmt
        .query_map([], |r| {
            Ok(PairStat {
                canonical_id: r.get(0)?,
                n: r.get(1)?,
                max_edge: r.get::<_, f64>(2).unwrap_or(0.0),
                avg_edge: r.get::<_, f64>(3).unwrap_or(0.0),
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    for ps in &pair_stats {
        let persist_label = match ps.n {
            1 => "single",
            2 => "2 cycles",
            3..=5 => "short",
            6..=20 => "medium",
            _ => "long",
        };
        let id_short = if ps.canonical_id.len() > 38 {
            format!("{}…", &ps.canonical_id[..37])
        } else {
            ps.canonical_id.clone()
        };
        println!(
            "  {:<40} {:>8} {:>8.2} {:>8.2} {:>8}",
            id_short, ps.n,
            ps.max_edge * 100.0,
            ps.avg_edge * 100.0,
            persist_label
        );
    }

    // ── Section 5: Threshold recommendations ─────────────────────────────────

    println!("\n── Threshold Recommendations ──────────────────────────────────");

    if edges.len() >= 5 {
        let p25_edge = percentile(&edges, 25.0);
        let p75_edge = percentile(&edges, 75.0);

        println!("  Based on current data:");
        println!("  - Current min_net_edge in config.yaml applies to live detection.");
        println!(
            "  - p25 net edge is {:.2}% — opportunities below this are thin.",
            p25_edge * 100.0
        );
        println!(
            "  - p75 net edge is {:.2}% — good starting threshold for live trading.",
            p75_edge * 100.0
        );

        if false_positive_pct > 50.0 {
            println!(
                "  - {:.0}% of canonical IDs appeared only once. Consider raising \
                 persistence_snapshots in config.yaml (currently filtering single-tick noise).",
                false_positive_pct
            );
        } else if false_positive_pct < 10.0 {
            println!(
                "  - Only {:.0}% single-tick false positives. Persistence filter is well-tuned.",
                false_positive_pct
            );
        }
    } else {
        println!("  Insufficient data for threshold recommendations.");
        println!("  Run arb_monitor for at least 1 hour before evaluating.");
    }

    // ── Section 6: Orders and fills (if any) ─────────────────────────────────

    let order_count: i64 = conn.query_row("SELECT COUNT(*) FROM orders", [], |r| r.get(0))?;
    let fill_count: i64 = conn.query_row("SELECT COUNT(*) FROM fills", [], |r| r.get(0))?;

    if order_count > 0 {
        println!("\n── Orders & Fills ─────────────────────────────────────────────");
        println!("  Total orders: {}", order_count);
        println!("  Total fills:  {}", fill_count);

        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM orders GROUP BY status ORDER BY COUNT(*) DESC",
        )?;
        let order_statuses: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        for (status, count) in &order_statuses {
            println!("    {}: {}", status, count);
        }

        if fill_count > 0 {
            let total_fees: f64 = conn
                .query_row(
                    "SELECT SUM(CAST(fee AS REAL)) FROM fills",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0.0);
            println!("  Total fees paid: ${:.4}", total_fees);
        }
    }

    println!("\n  Done. Review opportunities in data/arb.db for full detail.");
    Ok(())
}
