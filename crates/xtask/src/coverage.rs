//! The coverage gate (§3.2): `notify-domain`/`notify-app` stay ≥ 90 %, and ≥ 80 % of the
//! lines a change touched are covered. No global-percentage gate: it is easy to game.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Context as _;

/// Lines are covered when their LCOV hit count is non-zero.
type Hits = BTreeMap<u32, u64>;

/// Per-file line coverage, keyed by the path LCOV recorded.
#[derive(Debug, Default)]
pub(crate) struct Coverage {
    files: BTreeMap<String, Hits>,
}

impl Coverage {
    /// Parse the LCOV subset that matters: `SF:`, `DA:<line>,<hits>`, `end_of_record`.
    pub(crate) fn from_lcov(text: &str) -> Self {
        let mut files = BTreeMap::new();
        let mut current: Option<(String, Hits)> = None;

        for line in text.lines() {
            if let Some(path) = line.strip_prefix("SF:") {
                current = Some((path.to_owned(), Hits::new()));
            } else if let Some(record) = line.strip_prefix("DA:") {
                let Some((number, hits)) = record.split_once(',') else {
                    continue;
                };
                let (Ok(number), Ok(hits)) = (number.parse::<u32>(), hits.parse::<u64>()) else {
                    continue;
                };
                if let Some((_, lines)) = current.as_mut() {
                    // Keep the best hit count: an uncalled monomorphisation must not mask a called one.
                    lines
                        .entry(number)
                        .and_modify(|e| *e = (*e).max(hits))
                        .or_insert(hits);
                }
            } else if line.starts_with("end_of_record")
                && let Some((path, lines)) = current.take()
            {
                files.insert(path, lines);
            }
        }

        Self { files }
    }

    /// Ratio of covered lines across files whose path contains a needle; `None` when nothing
    /// matched (an absent measurement is not 0 %).
    fn ratio_for(&self, needles: &[&str]) -> Option<(u64, u64)> {
        let mut total = 0;
        let mut covered = 0;

        for (path, lines) in &self.files {
            if !needles.iter().any(|needle| path.contains(needle)) {
                continue;
            }
            for hits in lines.values() {
                total += 1;
                if *hits > 0 {
                    covered += 1;
                }
            }
        }

        (total > 0).then_some((covered, total))
    }

    /// Coverage restricted to the lines listed in `touched`, keyed by repo-relative path.
    fn ratio_for_touched(&self, touched: &BTreeMap<String, BTreeSet<u32>>) -> Option<(u64, u64)> {
        let mut total = 0;
        let mut covered = 0;

        for (path, wanted) in touched {
            // LCOV paths may be absolute; match on the suffix so both forms work.
            let Some((_, lines)) = self
                .files
                .iter()
                .find(|(recorded, _)| recorded.ends_with(path))
            else {
                continue;
            };
            for line in wanted {
                // No LCOV entry means not executable (comment, blank, `}`); it can't count against us.
                if let Some(hits) = lines.get(line) {
                    total += 1;
                    if *hits > 0 {
                        covered += 1;
                    }
                }
            }
        }

        (total > 0).then_some((covered, total))
    }
}

fn percent((covered, total): (u64, u64)) -> f64 {
    if total == 0 {
        return 100.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "line counts stay far below 2^53"
    )]
    let ratio = covered as f64 / total as f64;
    ratio * 100.0
}

/// Lines added or modified by `git diff <base>...HEAD`, per file.
///
/// Parsed from `--unified=0` hunk headers; only the `+` side matters.
fn touched_lines(base: &str) -> anyhow::Result<BTreeMap<String, BTreeSet<u32>>> {
    let output = std::process::Command::new("git")
        .args([
            "diff",
            "--unified=0",
            "--no-color",
            &format!("{base}...HEAD"),
        ])
        .output()
        .context("running git diff")?;

    anyhow::ensure!(
        output.status.success(),
        "git diff against {base} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let diff = String::from_utf8_lossy(&output.stdout);
    let mut touched: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut path: Option<String> = None;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            let is_rust = Path::new(rest)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"));
            path = is_rust.then(|| rest.to_owned());
        } else if let Some(header) = line.strip_prefix("@@ ")
            && let Some(current) = path.as_ref()
        {
            // "@@ -12,0 +13,4 @@" -> start 13, count 4. A missing count means 1.
            let Some(added) = header.split_whitespace().find_map(|f| f.strip_prefix('+')) else {
                continue;
            };
            let (start, count) = match added.split_once(',') {
                Some((start, count)) => (start.parse::<u32>(), count.parse::<u32>().unwrap_or(1)),
                None => (added.parse::<u32>(), 1),
            };
            let Ok(start) = start else { continue };
            touched
                .entry(current.clone())
                .or_default()
                .extend(start..start + count);
        }
    }

    Ok(touched)
}

/// The floor for pure domain and application logic.
const DOMAIN_FLOOR: f64 = 90.0;
/// The floor for lines this change touched.
const DIFF_FLOOR: f64 = 80.0;

/// Run both gates, print a markdown summary, and fail if either floor is missed.
pub(crate) fn gate(lcov: &Path, base: Option<&str>) -> anyhow::Result<()> {
    let text =
        std::fs::read_to_string(lcov).with_context(|| format!("reading {}", lcov.display()))?;
    let coverage = Coverage::from_lcov(&text);

    let mut failures = Vec::new();
    println!("## Coverage\n");
    println!("| Gate | Result | Floor |");
    println!("| --- | --- | --- |");

    match coverage.ratio_for(&["notify-domain/src", "notify-app/src"]) {
        Some(ratio) => {
            let value = percent(ratio);
            let verdict = if value >= DOMAIN_FLOOR {
                "pass"
            } else {
                "**FAIL**"
            };
            println!(
                "| Domain + app | {value:.1} % ({}/{} lines) — {verdict} | {DOMAIN_FLOOR:.0} % |",
                ratio.0, ratio.1
            );
            if value < DOMAIN_FLOOR {
                failures.push(format!(
                    "domain and app coverage is {value:.1} %, below the {DOMAIN_FLOOR:.0} % floor"
                ));
            }
        }
        None => println!(
            "| Domain + app | not measured yet (no instrumented lines) | {DOMAIN_FLOOR:.0} % |"
        ),
    }

    if let Some(base) = base {
        let touched = touched_lines(base)?;
        match coverage.ratio_for_touched(&touched) {
            Some(ratio) => {
                let value = percent(ratio);
                let verdict = if value >= DIFF_FLOOR {
                    "pass"
                } else {
                    "**FAIL**"
                };
                println!(
                    "| Lines changed | {value:.1} % ({}/{} lines) — {verdict} | {DIFF_FLOOR:.0} % |",
                    ratio.0, ratio.1
                );
                if value < DIFF_FLOOR {
                    failures.push(format!(
                        "only {value:.1} % of the lines this change touched are covered, \
                         below the {DIFF_FLOOR:.0} % floor"
                    ));
                }
            }
            None => {
                println!(
                    "| Lines changed | no executable Rust lines touched | {DIFF_FLOOR:.0} % |"
                );
            }
        }
    }

    if failures.is_empty() {
        println!("\nAll coverage gates pass.");
        return Ok(());
    }

    println!();
    for failure in &failures {
        println!("- {failure}");
    }
    anyhow::bail!("{} coverage gate(s) failed", failures.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
SF:/repo/crates/notify-domain/src/lib.rs
DA:1,1
DA:2,0
DA:3,5
end_of_record
SF:/repo/crates/notify-adapters/http-actix/src/lib.rs
DA:1,0
DA:2,0
end_of_record
";

    #[test]
    fn parses_lcov_line_records() {
        let coverage = Coverage::from_lcov(SAMPLE);
        assert_eq!(coverage.files.len(), 2);
        assert_eq!(coverage.ratio_for(&["notify-domain/src"]), Some((2, 3)));
    }

    #[test]
    fn scope_excludes_files_outside_the_needles() {
        let coverage = Coverage::from_lcov(SAMPLE);
        // The adapter is 0/2 and must not drag the domain figure down.
        let value = percent(coverage.ratio_for(&["notify-domain/src"]).unwrap());
        assert!(
            (value - 200.0 / 3.0).abs() < 1e-9,
            "expected 66.7 %, got {value}"
        );
    }

    #[test]
    fn absent_measurement_is_not_zero_percent() {
        let coverage = Coverage::from_lcov(SAMPLE);
        assert!(coverage.ratio_for(&["notify-cli/src"]).is_none());
    }

    #[test]
    fn repeated_lines_keep_the_best_hit_count() {
        let coverage = Coverage::from_lcov(
            "SF:/repo/crates/notify-domain/src/a.rs\nDA:1,0\nDA:1,7\nend_of_record\n",
        );
        assert_eq!(coverage.ratio_for(&["notify-domain"]), Some((1, 1)));
    }

    #[test]
    fn touched_lines_without_an_lcov_entry_are_not_counted() {
        let coverage = Coverage::from_lcov(SAMPLE);
        let mut touched = BTreeMap::new();
        touched.insert(
            "crates/notify-domain/src/lib.rs".to_owned(),
            BTreeSet::from([1, 2, 99]),
        );
        assert_eq!(coverage.ratio_for_touched(&touched), Some((1, 2)));
    }
}
