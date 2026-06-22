//! KAT + benchmark for the GPU Ajtai commitment (flux-fold prover acceleration).
//! Proves GPU commit == CPU commit and measures the speedup. Run on the GPU box.
use std::time::Instant;
const Q: u64 = 2_147_483_647;
fn cpu_commit(a: &[u64], w: &[u64], m: usize, n: usize) -> Vec<u64> {
    let mut c = vec![0u64; m];
    for i in 0..m { let row=&a[i*n..(i+1)*n]; let mut acc:u128=0; for j in 0..n { acc += row[j] as u128 * w[j] as u128; } c[i]=(acc % Q as u128) as u64; }
    c
}
fn main() -> anyhow::Result<()> {
    let m=256usize; let n=256usize; let count=8192usize;
    let a: Vec<u64> = (0..m*n).map(|k| (k as u64).wrapping_mul(2654435761).wrapping_add(1) % Q).collect();
    let wits: Vec<Vec<u64>> = (0..count).map(|wi| (0..n).map(|j| ((wi*131+j*17+7) as u64) % Q).collect()).collect();
    let t0=Instant::now();
    let cpu: Vec<Vec<u64>> = wits.iter().map(|w| cpu_commit(&a,w,m,n)).collect();
    let cpu_ms=t0.elapsed().as_secs_f64()*1000.0;
    let g = flux_miner::gpu::GpuBlake4::new()?;
    println!("GPU: {}", g.device_name);
    let _=g.ajtai_commit_batch(&a,&wits[..256],m,n)?; // warm
    let t1=Instant::now();
    let gpu = g.ajtai_commit_batch(&a,&wits,m,n)?;
    let gpu_ms=t1.elapsed().as_secs_f64()*1000.0;
    let ok = gpu==cpu;
    println!("AJTAI-GPU-KAT {} | m={m} n={n} M={count} | CPU {cpu_ms:.1}ms  GPU {gpu_ms:.2}ms  speedup {:.1}x",
             if ok {"PASS"} else {"FAIL"}, cpu_ms/gpu_ms.max(0.001));
    if !ok { for wi in 0..count { if gpu[wi]!=cpu[wi] { eprintln!("mismatch at witness {wi}"); break; } } std::process::exit(1); }
    Ok(())
}
