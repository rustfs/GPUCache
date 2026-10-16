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
        "nixl_pd=NOT_RUN_HARDWARE",
        "shared_dram=NOT_RUN_HARDWARE",
        "shared_nvme=NOT_RUN_HARDWARE",
        "rustfs_refill=NOT_RUN_HARDWARE",
        "ttft=NOT_RUN_HARDWARE",
        "tpot=NOT_RUN_HARDWARE",
        "goodput=NOT_RUN_HARDWARE",
        "samples=NONE",
        "numeric_tolerance=NOT_APPLICABLE",
        "bypass_when_no_reuse=MISS_RECOMPUTE",
        "mooncake_transfer_engine=NOT_LINKED",
        "mooncake_published_numbers=NOT_REPRODUCED",
        "rdma_fallback=TCP_CPU_CONTRACT",
    ];
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// V3 paired gate. The required baseline is NIC RDMA, not a weak TCP path.
/// Every measured row stays `NOT_RUN_HARDWARE`. Cost stays `TCO_UNKNOWN`.
/// The report has no digits, so it cannot publish a TTFT, a watt, or a token rate.
pub fn v3_paired_report() -> String {
    let lines = [
        "status=NOT_RUN_HARDWARE",
        "comparison=NIC_RDMA_BASELINE_VS_DPU",
        "nic_rdma_baseline=NOT_RUN_HARDWARE",
        "dpu_storage_queue=NOT_RUN_HARDWARE",
        "tcp_used_as_dpu_baseline=FORBIDDEN",
        "same_gpu=REQUIRED_NOT_RUN",
        "same_model=REQUIRED_NOT_RUN",
        "same_trace=REQUIRED_NOT_RUN",
        "same_slo=REQUIRED_NOT_RUN",
        "same_capacity=REQUIRED_NOT_RUN",
        "same_auth=REQUIRED_NOT_RUN",
        "ttft=NOT_RUN_HARDWARE",
        "tpot=NOT_RUN_HARDWARE",
        "itl=NOT_RUN_HARDWARE",
        "goodput=NOT_RUN_HARDWARE",
        "host_core_seconds=NOT_RUN_HARDWARE",
        "dpu_cpu=NOT_RUN_HARDWARE",
        "dpu_dram=NOT_RUN_HARDWARE",
        "gpu_interference=NOT_RUN_HARDWARE",
        "wire_bytes=NOT_RUN_HARDWARE",
        "ssd_write_amp=NOT_RUN_HARDWARE",
        "wall_power=NOT_RUN_HARDWARE",
        "tco=TCO_UNKNOWN",
        "go_gate=NOT_EVALUATED",
        "no_go_keeps_portable_backend=YES",
        "soak=NOT_RUN_HARDWARE",
        "pb_scale=NOT_CLAIMED",
        "production_ready=NOT_CLAIMED",
        "soft_roce=NOT_A_PERFORMANCE_RESULT",
        "mooncake_rdma=NOT_A_PERFORMANCE_RESULT",
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
    use super::{paired_baseline_report, v3_paired_report};

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
            "nixl_pd=NOT_RUN_HARDWARE",
            "shared_dram=NOT_RUN_HARDWARE",
            "shared_nvme=NOT_RUN_HARDWARE",
            "rustfs_refill=NOT_RUN_HARDWARE",
            "ttft=NOT_RUN_HARDWARE",
            "tpot=NOT_RUN_HARDWARE",
            "goodput=NOT_RUN_HARDWARE",
            "samples=NONE",
            "mooncake_transfer_engine=NOT_LINKED",
            "mooncake_published_numbers=NOT_REPRODUCED",
            "rdma_fallback=TCP_CPU_CONTRACT",
        ] {
            assert!(report.contains(key), "missing {key}");
        }
        assert!(!report.contains("tokens/s"));
        assert!(!report.contains("PASS"));
    }

    #[test]
    fn v3_report_requires_nic_rdma_and_invents_nothing() {
        let report = v3_paired_report();
        assert!(report.chars().all(|c| !c.is_ascii_digit()));
        for key in [
            "comparison=NIC_RDMA_BASELINE_VS_DPU",
            "nic_rdma_baseline=NOT_RUN_HARDWARE",
            "dpu_storage_queue=NOT_RUN_HARDWARE",
            "tcp_used_as_dpu_baseline=FORBIDDEN",
            "tco=TCO_UNKNOWN",
            "go_gate=NOT_EVALUATED",
            "pb_scale=NOT_CLAIMED",
            "production_ready=NOT_CLAIMED",
            "soft_roce=NOT_A_PERFORMANCE_RESULT",
            "mooncake_rdma=NOT_A_PERFORMANCE_RESULT",
        ] {
            assert!(report.contains(key), "missing {key}");
        }
        assert!(!report.contains("tokens/s"));
        assert!(!report.contains("PASS"));
    }
}
