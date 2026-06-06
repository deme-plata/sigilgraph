// Standalone proof: incremental O(1) vs whole-map O(n) rehash, the Stargate #1 win.
use std::time::Instant;
use std::collections::BTreeMap;
use sigil_state::Accumulator;

fn whole_map_rehash(map:&BTreeMap<Vec<u8>,Vec<u8>>)->[u8;32]{
    let mut h=blake3::Hasher::new();
    for (k,v) in map { h.update(k); h.update(v); }
    *h.finalize().as_bytes()
}
fn main(){
    for n in [1_000usize,10_000,100_000]{
        let mut map=BTreeMap::new();
        let mut acc=Accumulator::new();
        for i in 0..n{ let k=(i as u64).to_le_bytes().to_vec(); let v=(i as u128).to_le_bytes().to_vec();
            map.insert(k.clone(),v.clone()); acc.insert(&k,&v); }
        // simulate 1000 blocks, each touches ONE leaf then computes a root
        let key=(0u64).to_le_bytes().to_vec();
        // OLD: whole-map rehash per block
        let t=Instant::now();
        for b in 0..1000u128{ let nv=b.to_le_bytes().to_vec(); map.insert(key.clone(),nv); let _=whole_map_rehash(&map); }
        let old=t.elapsed();
        // NEW: incremental update + O(1) root per block
        let t=Instant::now();
        let mut cur=(0u128).to_le_bytes().to_vec();
        for b in 0..1000u128{ let nv=b.to_le_bytes().to_vec(); acc.update(&key,&cur,&nv); cur=nv; let _=acc.root(); }
        let inc=t.elapsed();
        println!("n={:>7}  whole-map={:>9.2}ms  incremental={:>7.3}ms  speedup={:>6.0}x",
            n, old.as_secs_f64()*1e3, inc.as_secs_f64()*1e3, old.as_secs_f64()/inc.as_secs_f64());
    }
}
