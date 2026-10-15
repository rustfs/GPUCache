// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Paired baseline names for the V1 exit criteria.
//!
//! This crate does not run a GPU, an engine, or a timer. It prints
//! `NOT_RUN_HARDWARE` for every row that needs token goodput. Filling in a
//! number here would be a fabricated result.

/// Text report. It contains no digits, so it cannot smuggle a latency or a token rate.
pub fn paired_baseline_report() -> String {
    let lines = [
        "status=NOT_RUN_HARDWARE",
        "engine_native_prefix_cache=NOT_RUN_HARDWARE",
        "local_offload=NOT_RUN_HARDWARE",
        "gpucache_cpu_tcp_goodput=NOT_RUN_HARDWARE",
        "scenario_cold=NOT_RUN_HARDWARE",
        "scenario_warm=NOT_RUN_HARDWARE",
        "scenario_no_reuse=NOT_RUN_HARDWARE",
        "scenario_multi_turn=NOT_RUN_HARDWARE",
        "scenario_hbm_within=NOT_RUN_HARDWARE",
        "scenario_hbm_cliff=NOT_RUN_HARDWARE",
        "write_cost=NOT_RUN_HARDWARE",
        "samples=NONE",
        "numeric_tolerance=NOT_APPLICABLE",
        "bypass_when_no_reuse=MISS_RECOMPUTE",
    ];
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::paired_baseline_report;

    #[test]
    fn report_has_no_invented_numbers() {
        let report = paired_baseline_report();
        assert!(report.chars().all(|c| !c.is_ascii_digit()));
        for key in [
            "engine_native_prefix_cache=NOT_RUN_HARDWARE",
            "local_offload=NOT_RUN_HARDWARE",
            "gpucache_cpu_tcp_goodput=NOT_RUN_HARDWARE",
            "scenario_cold=NOT_RUN_HARDWARE",
            "scenario_warm=NOT_RUN_HARDWARE",
            "scenario_no_reuse=NOT_RUN_HARDWARE",
            "scenario_multi_turn=NOT_RUN_HARDWARE",
            "scenario_hbm_within=NOT_RUN_HARDWARE",
            "scenario_hbm_cliff=NOT_RUN_HARDWARE",
            "samples=NONE",
        ] {
            assert!(report.contains(key), "missing {key}");
        }
        assert!(!report.contains("tokens/s"));
        assert!(!report.contains("PASS"));
    }
}
