//! Box GPU fold-prover: fold N chronos block-witnesses fully on the GPU via the
//! fused gpu_fold (resident W/C buffers), proof bit-identical to CPU. Default 100k blocks.
//! Challenge uses one rayon-parallel blake3 over the byte-cast commitment buffer
//! (was 25.6M single-u64 update() calls — the host bottleneck).
use std::time::Instant;
use flux_fold::{Ajtai, fold, FoldedProof, Q};
fn block_witness(seed:u64, n:usize)->Vec<u64>{
    let mut h=blake3::Hasher::new(); h.update(b"sigil-chronos/fold-block-witness/v1"); h.update(&seed.to_le_bytes());
    let mut xof=h.finalize_xof(); let mut buf=vec![0u8;n*8]; xof.fill(&mut buf);
    (0..n).map(|j|{let mut a=[0u8;8]; a.copy_from_slice(&buf[j*8..j*8+8]); u64::from_le_bytes(a)%Q}).collect()
}
fn mulm(a:u64,b:u64)->u64{((a as u128*b as u128)%Q as u128)as u64}
// SAME hash as flux_fold::challenge on little-endian: domain + every commit u64 as LE bytes,
// in order. cast_slice(c) == concatenation of x.to_le_bytes() on x86. One rayon-parallel call.
fn challenge_flat(c:&[u64])->u64{
    let bytes = unsafe { std::slice::from_raw_parts(c.as_ptr() as *const u8, std::mem::size_of_val(c)) };
    let mut h=blake3::Hasher::new(); h.update(b"flux-fold/rho/v1"); h.update_rayon(bytes);
    let d=h.finalize();
    (u64::from_le_bytes(d.as_bytes()[0..8].try_into().unwrap())%(Q-1))+1
}
fn main()->anyhow::Result<()>{
    let (m,n)=(256usize,256usize);
    let count:usize=std::env::args().nth(1).and_then(|s|s.parse().ok()).unwrap_or(100_000);
    let ajtai=Ajtai::from_seed(m,n,&[7u8;32]);
    let wits:Vec<Vec<u64>>=(0..count).map(|i|block_witness(i as u64,n)).collect();
    let t0=Instant::now(); let cpu=fold(&ajtai,&wits); let cpu_ms=t0.elapsed().as_secs_f64()*1000.0;
    let g=flux_miner::gpu::GpuBlake4::new()?;
    let a=ajtai.matrix();
    let w_flat:Vec<u64>=wits.iter().flatten().copied().collect();
    let rho_cb=|c_flat:&[u64]|{ let rho=challenge_flat(c_flat); let mut rp=vec![1u64;count]; for i in 1..count { rp[i]=mulm(rp[i-1],rho); } rp };
    let _=g.gpu_fold(a,&w_flat,m,n,count,&rho_cb)?; // warm
    let t1=Instant::now();
    let (c_star,w_star)=g.gpu_fold(a,&w_flat,m,n,count,&rho_cb)?;
    let gpu_ms=t1.elapsed().as_secs_f64()*1000.0;
    let gpu=FoldedProof{c_star,w_star,m_count:count};
    let ok=gpu.c_star==cpu.c_star && gpu.w_star==cpu.w_star;
    println!("CHRONOS-FOLD-GPU {} | blocks={count} | CPU {cpu_ms:.0}ms GPU {gpu_ms:.0}ms speedup {:.1}x => {:.0} blocks/sec",
             if ok {"MATCH"} else {"MISMATCH"}, cpu_ms/gpu_ms.max(0.001), (count as f64)/(gpu_ms/1000.0));
    if !ok { std::process::exit(1); } Ok(())
}
