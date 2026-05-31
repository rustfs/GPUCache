# RustFS GPUCache 🚀

[![Rust](https://img.shields.io/badge/Language-Rust-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Status](https://img.shields.io/badge/Status-Active_Development-brightgreen.svg)]()

**GPUCache** is an AI-native, highly optimized KV Cache offloading layer designed specifically for LLM inference and Agentic AI workloads. Built entirely in Rust and engineered for the NVIDIA BlueField DPU ecosystem, it transforms standard EBOF (Ethernet Bunch of Flash) NVMe nodes into a shared, persistent, and hyper-fast L4 context memory pool for GPU clusters.

We are redefining the **NVIDIA G3.5 Memory Tier**.

---

## 💡 The Vision: Breaking the "Memory Wall"

In large language model (LLM) inference—especially with long contexts and complex Agent frameworks—GPU HBM (High Bandwidth Memory) capacity is the primary bottleneck. When the KV Cache exceeds VRAM limits, it gets evicted, forcing the GPU to endure the massive "Recompute Tax" when the same context is needed again.

Traditional storage is too slow (software overhead, CPU bottlenecks, TCP/IP stack delays). 

**GPUCache** solves this by pushing the intelligence directly into the network edge. By offloading KV Cache to an EBOF node powered by an ARM-based DPU, we eliminate the x86 host CPU and RAM entirely. GPUs can read/write context directly to NVMe flash over the network as if it were local memory.

## 🏗️ Technical Architecture (The RustFS Edge)

GPUCache is designed with a radical "Hardware-Software Co-design" philosophy:

* **Zero-GC, 100% Rust:** Unlike other emerging solutions (e.g., MinIO MemKV) that rely on garbage-collected languages, GPUCache is written in Rust. This guarantees predictable, microsecond-level tail latencies and minimal memory footprint—crucial when running on the highly constrained ARM cores of a DPU.
* **Serverless EBOF Architecture:** We completely bypass the traditional x86 storage node architecture. GPUCache runs directly on the NVIDIA BlueField-3/4 DPU, utilizing SPDK to directly manage the underlying NVMe SSDs via a PCIe switch.
* **End-to-End RDMA (RoCEv2):** Context data travels directly from the GPU VRAM over the Spectrum-X network to the DPU, and straight into flash. Zero host-CPU intervention on both the compute and storage sides.
* **Hardware Erasure Coding (EC) Offload:** Instead of wasteful multi-replica setups, we utilize Erasure Coding to maximize storage density. To maintain line-rate performance (400G/800G), the EC parity calculations are offloaded directly to the DPU's hardware acceleration engines via DOCA.
* **Smart Pre-fetching & Eviction:** GPUCache is not just a block device; it is a KV-aware engine. It understands the access patterns of LLM frameworks (integrating with vLLM/TensorRT-LLM) to intelligently route and pre-fetch PageAttention blocks.

## ⚔️ GPUCache vs. The Status Quo

| Feature | Traditional Storage | MinIO MemKV (Go) | RustFS GPUCache (Rust) |
| :--- | :--- | :--- | :--- |
| **Language** | C / C++ / Java | Go (Subject to GC pauses) | **Rust (Zero-cost abstraction, deterministic)** |
| **Data Path** | CPU -> Memory -> NIC | RDMA direct to DPU | **RDMA direct to DPU** |
| **Storage Engine** | File / Object (S3) | Proprietary NixL | **KV-Optimized NVMe-oF** |
| **Resilience** | 3x Replication (Expensive) | Unknown/WIP | **Hardware-Offloaded Erasure Coding** |

## 🗺️ Roadmap & Milestones

- [ ] Core Rust KV engine implementation.
- [ ] Basic RDMA / RoCEv2 communication layer.
- [ ] Integration with SPDK for direct NVMe addressing.
- [ ] NVIDIA DOCA API integration for ARM cross-compilation.
- [ ] DPU Hardware Erasure Coding (EC) offload module.

## 🤝 Call for Contributors

We are at the frontier of AI infrastructure. Building a CPU-less, DPU-driven storage layer is incredibly challenging and requires expertise across the entire stack. We are actively seeking brilliant minds to join us in this open-source journey.

We are specifically looking for contributors with expertise in:
1.  **Rust Systems Programming:** Low-level memory management, async runtimes, and lock-free data structures.
2.  **Network Engineering:** RDMA, RoCEv2, and Infiniband verbs.
3.  **Hardware Acceleration:** NVIDIA DOCA, BlueField DPUs, and ARM64 cross-compilation.
4.  **AI Inference Frameworks:** Developers familiar with vLLM, TensorRT-LLM, and PageAttention mechanisms to build the client-side interceptors.

### How to get involved:
* **Star the repo** to show your support!
* Check the [Issues](https://github.com/rustfs/gpucache/issues) tab for `good first issue` or `help wanted` tags.
* Join our community discussions in the [Discussions](https://github.com/rustfs/gpucache/discussions) tab to propose architectural improvements.
* Fork the repository and submit your PRs!

## 📄 License

This project is licensed under the Apache 2.0 License - see the [LICENSE](LICENSE) file for details.

---
*Built with passion by the RustFS community. Let's make the Memory Wall a thing of the past.*
