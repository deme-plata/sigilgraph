//! gpu — CPU/GPU **hybrid dual-lane** mining, ported from the QUG q-miner's GPU
//! subsystem (`q-miner/src/gpu/*`, "Hybrid Quantum Mining" v10.2.0) and adapted
//! to SIGIL's dual lanes + the new BLAKE4 (`crate::pow`).
//!
//! ## The clean mapping
//!
//! SIGIL's two lanes split naturally across the two kinds of hardware:
//!
//! ```text
//!   Lane A — BLAKE4 (Φ, POWER)   → GPU   (embarrassingly parallel nonce search)
//!   Lane B — VDF    (Ω, TIME)    → CPU   (inherently sequential — GPU can't help)
//! ```
//!
//! So the GPU does what it is good at (millions of independent BLAKE4 hashes) and
//! the CPU does the one thing that *must* be sequential (the Wesolowski VDF over
//! the found nonce). The hybrid miner: GPU `search()` → winning nonce → CPU
//! `flux_vdf::eval` over `vdf_seed(header, nonce)` → assemble the `DualLaneBlock`.
//! Multiple GPUs + CPU lanes can run concurrently against disjoint nonce ranges.
//!
//! ## Status
//!
//! ⚠️ **Scaffold — gated behind the `gpu` feature (OFF by default), UNVALIDATED
//! until run on real GPU hardware.** The OpenCL kernel (`blake4.cl`) is a
//! byte-for-byte port of [`crate::pow::compress8`]; the FIRST thing to do on a GPU
//! box is a KAT: confirm the kernel's word for `(header, nonce, rounds=7)` equals
//! [`crate::pow::blake4_word`]. Epsilon has no GPU; validate on a Vast box
//! (propose-only). CUDA / Vulkan backends (q-miner has both) are follow-ons; this
//! ports the OpenCL backend first as it is the most portable.

use opencl3::command_queue::CommandQueue;
use opencl3::context::Context;
use opencl3::device::{Device, CL_DEVICE_TYPE_ALL, CL_DEVICE_TYPE_GPU};
use opencl3::platform::get_platforms;
use opencl3::kernel::{ExecuteKernel, Kernel};
use opencl3::memory::{Buffer, CL_MEM_READ_ONLY, CL_MEM_WRITE_ONLY};
use opencl3::program::Program;
use opencl3::types::{cl_int, cl_uint, cl_ulong, CL_BLOCKING};
use std::ptr;

const KERNEL_SRC: &str = include_str!("blake4.cl");
const KERNEL_NAME: &str = "blake4_search";
const WORDS_KERNEL_NAME: &str = "blake4_words";

/// A discovered GPU.
#[derive(Clone, Debug)]
pub struct GpuDeviceInfo {
    pub name: String,
    pub global_mem_mb: u64,
    pub max_work_group: usize,
}

/// OpenCL devices, GPU-first then ANY. Enumerates platforms ourselves and
/// IGNORES per-platform errors — opencl3's 1-arg `get_all_devices(type)` does
/// `for p { p.get_devices(type)? }`, so the `?` aborts the WHOLE call if ANY
/// platform reports `CL_DEVICE_NOT_FOUND` (e.g. a Windows box with NVIDIA + an
/// iGPU platform that has no GPU-typed device). That bug made multi-platform
/// Windows boxes report "no platforms" while the NVIDIA GPU was right there.
/// (This is the q-miner approach — proven on Windows.)
fn pick_device_ids() -> Vec<opencl3::device::cl_device_id> {
    let platforms = match get_platforms() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let mut gpu = Vec::new();
    let mut any = Vec::new();
    for p in &platforms {
        if let Ok(d) = p.get_devices(CL_DEVICE_TYPE_GPU) {
            gpu.extend(d);
        }
        if let Ok(d) = p.get_devices(CL_DEVICE_TYPE_ALL) {
            any.extend(d);
        }
    }
    if !gpu.is_empty() {
        gpu
    } else {
        any
    }
}

/// Enumerate OpenCL devices (empty if none / no OpenCL runtime).
pub fn list_devices() -> Vec<GpuDeviceInfo> {
    let mut out = Vec::new();
    {
        let ids = pick_device_ids();
        for id in ids {
            let d = Device::new(id);
            out.push(GpuDeviceInfo {
                name: d.name().unwrap_or_else(|_| "unknown".into()),
                global_mem_mb: d.global_mem_size().unwrap_or(0) / (1024 * 1024),
                max_work_group: d.max_work_group_size().unwrap_or(0),
            });
        }
    }
    out
}

/// Build the 16-word (64-byte) base message block exactly as
/// [`crate::pow::blake4_word`] lays it out: header at `0..hlen`, the 8 nonce
/// bytes are spliced by the kernel at `hlen`, the rest zero. `hlen <= 56`.
fn build_base_m(header: &[u8]) -> ([cl_uint; 16], u32) {
    let hlen = header.len().min(56);
    let mut buf = [0u8; 64];
    buf[..hlen].copy_from_slice(&header[..hlen]);
    let mut m = [0u32; 16];
    for i in 0..16 {
        m[i] = u32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
    }
    (m, hlen as u32)
}

/// An OpenCL BLAKE4 Lane-A search engine (one device).
pub struct GpuBlake4 {
    context: Context,
    queue: CommandQueue,
    kernel: Kernel,
    words_kernel: Kernel,
    pub device_name: String,
}

impl GpuBlake4 {
    /// Initialise the first GPU + build the kernels, with a STEP-BY-STEP trace so
    /// any failure names the exact OpenCL call + error (and the NVIDIA build log
    /// for kernel-build failures). The full trace is written to `sigil-gpu-init.log`
    /// and, on failure, returned in the error.
    pub fn new() -> anyhow::Result<Self> {
        let mut t = String::from("[sigil-gpu init trace]\n");
        macro_rules! step {
            ($($a:tt)*) => {{ t.push_str(&format!($($a)*)); t.push('\n'); }};
        }
        let fail = |t: &str, what: &str| -> anyhow::Error {
            let _ = std::fs::write("sigil-gpu-init.log", t);
            anyhow::anyhow!("{}{}", t, what)
        };

        let platforms = match get_platforms() {
            Ok(p) => p,
            Err(e) => return Err(fail(&t, &format!("get_platforms FAILED: {e}"))),
        };
        step!("platforms found: {}", platforms.len());
        for p in &platforms {
            step!("  - {}", p.name().unwrap_or_else(|_| "?".into()));
        }
        let ids = pick_device_ids();
        step!("devices picked (GPU-first/any): {}", ids.len());
        let id = match ids.first() {
            Some(id) => *id,
            None => return Err(fail(&t, "no OpenCL device found")),
        };
        let device = Device::new(id);
        let device_name = device.name().unwrap_or_else(|_| "unknown".into());
        step!("device: {device_name}");
        let context = match Context::from_device(&device) {
            Ok(c) => c,
            Err(e) => return Err(fail(&t, &format!("clCreateContext FAILED: {e}"))),
        };
        step!("context: ok");
        let queue = match CommandQueue::create_default(&context, 0) {
            Ok(q) => q,
            Err(e) => return Err(fail(&t, &format!("create_command_queue FAILED: {e}"))),
        };
        step!("queue: ok");
        let program = match Program::create_and_build_from_source(&context, KERNEL_SRC, "") {
            Ok(p) => p,
            Err(buildlog) => {
                return Err(fail(&t, &format!("clBuildProgram FAILED — NVIDIA build log:\n{buildlog}")))
            }
        };
        step!("program: built");
        let kernel = match Kernel::create(&program, KERNEL_NAME) {
            Ok(k) => k,
            Err(e) => return Err(fail(&t, &format!("clCreateKernel({KERNEL_NAME}) FAILED: {e}"))),
        };
        let words_kernel = match Kernel::create(&program, WORDS_KERNEL_NAME) {
            Ok(k) => k,
            Err(e) => return Err(fail(&t, &format!("clCreateKernel({WORDS_KERNEL_NAME}) FAILED: {e}"))),
        };
        step!("kernels: ok");
        step!("GPU INIT OK → {device_name}");
        let _ = std::fs::write("sigil-gpu-init.log", &t); // success trace too
        Ok(Self { context, queue, kernel, words_kernel, device_name })
    }

    /// Compute the BLAKE4 word for `count` consecutive nonces (from `nonce_base`)
    /// on the GPU. Used by the KAT self-test.
    pub fn words(
        &self,
        header: &[u8],
        rounds: u32,
        nonce_base: u64,
        count: usize,
    ) -> anyhow::Result<Vec<u64>> {
        let (base_m, hlen) = build_base_m(header);
        let block_len = hlen + 8;

        let mut base_buf = unsafe {
            Buffer::<cl_uint>::create(&self.context, CL_MEM_READ_ONLY, 16, ptr::null_mut())?
        };
        let mut out = unsafe {
            Buffer::<cl_ulong>::create(&self.context, CL_MEM_WRITE_ONLY, count, ptr::null_mut())?
        };
        unsafe {
            self.queue.enqueue_write_buffer(&mut base_buf, CL_BLOCKING, 0, &base_m, &[])?;
            ExecuteKernel::new(&self.words_kernel)
                .set_arg(&base_buf)
                .set_arg(&hlen)
                .set_arg(&(nonce_base as cl_ulong))
                .set_arg(&(rounds as cl_uint))
                .set_arg(&(block_len as cl_uint))
                .set_arg(&out)
                .set_global_work_size(count)
                .enqueue_nd_range(&self.queue)?
                .wait()?;
        }
        let mut host = vec![0 as cl_ulong; count];
        unsafe {
            self.queue.enqueue_read_buffer(&out, CL_BLOCKING, 0, &mut host, &[])?;
        }
        Ok(host.into_iter().map(|w| w as u64).collect())
    }

    /// On-hardware KAT: the GPU kernel's word MUST equal `pow::blake4_word` for
    /// the same (header, nonce, rounds). Checks R=7 (the BLAKE3 anchor) + a
    /// reduced round over a batch of nonces. Returns Ok(true) iff all match.
    pub fn selftest(&self) -> anyhow::Result<bool> {
        let header = [0x5au8; 32];
        let count = 256;
        let mut all_ok = true;
        for rounds in [crate::pow::FULL_ROUNDS, 3] {
            let gpu = self.words(&header, rounds, 0, count)?;
            for (i, g) in gpu.iter().enumerate() {
                let cpu = crate::pow::blake4_word(&header, i as u64, rounds);
                if *g != cpu {
                    eprintln!("  ✗ KAT mismatch R={rounds} nonce={i}: gpu={g:#018x} cpu={cpu:#018x}");
                    all_ok = false;
                    break;
                }
            }
        }
        Ok(all_ok)
    }

    /// Lane A: search `batch` nonces from `nonce_base` for one whose
    /// BLAKE4(header‖nonce) word `<= target` at `rounds` rounds. Returns the
    /// winning nonce, or `None` if the whole batch missed.
    pub fn search(
        &self,
        header: &[u8],
        target: u64,
        rounds: u32,
        nonce_base: u64,
        batch: usize,
    ) -> anyhow::Result<Option<u64>> {
        let (base_m, hlen) = build_base_m(header);
        let block_len = hlen + 8;

        let mut base_buf = unsafe {
            Buffer::<cl_uint>::create(&self.context, CL_MEM_READ_ONLY, 16, ptr::null_mut())?
        };
        let mut found_nonce = unsafe {
            Buffer::<cl_ulong>::create(&self.context, CL_MEM_WRITE_ONLY, 1, ptr::null_mut())?
        };
        let mut found_flag = unsafe {
            Buffer::<cl_int>::create(&self.context, CL_MEM_WRITE_ONLY, 1, ptr::null_mut())?
        };

        unsafe {
            self.queue.enqueue_write_buffer(&mut base_buf, CL_BLOCKING, 0, &base_m, &[])?;
            self.queue.enqueue_write_buffer(&mut found_flag, CL_BLOCKING, 0, &[0 as cl_int], &[])?;
            self.queue.enqueue_write_buffer(&mut found_nonce, CL_BLOCKING, 0, &[0 as cl_ulong], &[])?;

            ExecuteKernel::new(&self.kernel)
                .set_arg(&base_buf)
                .set_arg(&hlen)
                .set_arg(&(nonce_base as cl_ulong))
                .set_arg(&(target as cl_ulong))
                .set_arg(&(rounds as cl_uint))
                .set_arg(&(block_len as cl_uint))
                .set_arg(&found_nonce)
                .set_arg(&found_flag)
                .set_global_work_size(batch)
                .enqueue_nd_range(&self.queue)?
                .wait()?;
        }

        let mut flag = [0 as cl_int; 1];
        let mut nonce = [0 as cl_ulong; 1];
        unsafe {
            self.queue.enqueue_read_buffer(&found_flag, CL_BLOCKING, 0, &mut flag, &[])?;
            self.queue.enqueue_read_buffer(&found_nonce, CL_BLOCKING, 0, &mut nonce, &[])?;
        }
        Ok(if flag[0] != 0 { Some(nonce[0] as u64) } else { None })
    }
}
