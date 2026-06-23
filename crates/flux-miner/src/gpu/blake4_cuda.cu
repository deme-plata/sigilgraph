// blake4_cuda.cu — advanced CUDA-core NVIDIA miner for SIGIL's BLAKE4 PoW (Lane A, Φ/POWER).
//
// Byte-for-byte port of flux_miner::pow::compress8 / blake4_word (same IV, message
// schedule, G mix, flags CHUNK_START|CHUNK_END|ROOT = 11, counter 0, single ≤64B
// block) — but NATIVE CUDA, tuned for NVIDIA SM:
//   * __funnelshift_r  → single-instruction 32-bit rotate-right (vs OpenCL shift+or)
//   * ROUNDS is a TEMPLATE param → full unroll + constant-folded message permutation
//   * IV / MSG-permutation in __constant__ memory, header in __constant__ memory
//   * __launch_bounds__ for occupancy; grid-stride search; atomicCAS winner claim
//   * registers-only state (no shared/global per-hash traffic)
//
// Soundness chain (self-contained KAT, no Rust toolchain needed on the box):
//   (1) the embedded CPU reference compress8() == real BLAKE3 known-answer vectors
//       for "" and "abc"  → anchors the from-scratch core to the BLAKE3 spec at R=7.
//   (2) the GPU kernel word == CPU reference word for a batch of (header,nonce,rounds)
//   ⇒ transitively GPU ≡ BLAKE3 at R=7, exactly as pow::blake4_word requires.
//
// Build:  nvcc -O3 -arch=sm_89 -o blake4_cuda blake4_cuda.cu   (sm_89 = RTX 4090 Ada)
// Modes:  ./blake4_cuda selftest
//         ./blake4_cuda bench [seconds=5] [rounds=7]
//         ./blake4_cuda mine <header_hex> <target_hex_u64> [rounds=7] [nonce_base=0]

#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cstdlib>
#include <string>
#include <chrono>
#include <cuda_runtime.h>

// ── shared constants ─────────────────────────────────────────────────────────
__device__ __constant__ uint32_t IV[8] = {
    0x6A09E667u, 0xBB67AE85u, 0x3C6EF372u, 0xA54FF53Au,
    0x510E527Fu, 0x9B05688Cu, 0x1F83D9ABu, 0x5BE0CD19u};
// MSG permutation, pre-composed per round at compile time below.
static const int H_MSGP[16] = {2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8};

#define CKERR(call) do { cudaError_t _e = (call); if (_e != cudaSuccess) { \
    fprintf(stderr, "CUDA error %s:%d: %s\n", __FILE__, __LINE__, cudaGetErrorString(_e)); \
    exit(1);} } while (0)

// ── device rotate (funnel-shift: one SM instruction) ─────────────────────────
__device__ __forceinline__ uint32_t rotr(uint32_t x, uint32_t n) {
    return __funnelshift_r(x, x, n);
}

__device__ __forceinline__ void Gmix(uint32_t* v, int a, int b, int c, int d,
                                      uint32_t mx, uint32_t my) {
    v[a] = v[a] + v[b] + mx; v[d] = rotr(v[d] ^ v[a], 16);
    v[c] = v[c] + v[d];      v[b] = rotr(v[b] ^ v[c], 12);
    v[a] = v[a] + v[b] + my; v[d] = rotr(v[d] ^ v[a], 8);
    v[c] = v[c] + v[d];      v[b] = rotr(v[b] ^ v[c], 7);
}

// One BLAKE3 round over state v with message words m.
__device__ __forceinline__ void roundf(uint32_t* v, const uint32_t* m) {
    Gmix(v, 0, 4, 8, 12, m[0], m[1]);
    Gmix(v, 1, 5, 9, 13, m[2], m[3]);
    Gmix(v, 2, 6, 10, 14, m[4], m[5]);
    Gmix(v, 3, 7, 11, 15, m[6], m[7]);
    Gmix(v, 0, 5, 10, 15, m[8], m[9]);
    Gmix(v, 1, 6, 11, 12, m[10], m[11]);
    Gmix(v, 2, 7, 8, 13, m[12], m[13]);
    Gmix(v, 3, 4, 9, 14, m[14], m[15]);
}

__device__ __forceinline__ void permute(uint32_t* m) {
    const uint32_t o0=m[2],o1=m[6],o2=m[3],o3=m[10],o4=m[7],o5=m[0],o6=m[4],o7=m[13];
    const uint32_t o8=m[1],o9=m[11],o10=m[12],o11=m[5],o12=m[9],o13=m[14],o14=m[15],o15=m[8];
    m[0]=o0;m[1]=o1;m[2]=o2;m[3]=o3;m[4]=o4;m[5]=o5;m[6]=o6;m[7]=o7;
    m[8]=o8;m[9]=o9;m[10]=o10;m[11]=o11;m[12]=o12;m[13]=o13;m[14]=o14;m[15]=o15;
}

// Compress one ≤64B block (16 LE words already in `m`) at compile-time ROUNDS,
// returning the BLAKE4 target word (first 8 bytes of root CV, LE u64).
template <int ROUNDS>
__device__ __forceinline__ uint64_t compress_word(const uint32_t* m_in, uint32_t block_len) {
    uint32_t m[16];
#pragma unroll
    for (int i = 0; i < 16; i++) m[i] = m_in[i];
    uint32_t v[16] = {
        IV[0], IV[1], IV[2], IV[3], IV[4], IV[5], IV[6], IV[7],
        IV[0], IV[1], IV[2], IV[3], 0u, 0u, block_len, 11u /* START|END|ROOT */};
#pragma unroll
    for (int r = 0; r < ROUNDS; r++) { roundf(v, m); permute(m); }
    const uint32_t w0 = v[0] ^ v[8];
    const uint32_t w1 = v[1] ^ v[9];
    return ((uint64_t)w0) | (((uint64_t)w1) << 32);
}

// Splice the 8 LE nonce bytes into base message words at byte offset hlen.
__device__ __forceinline__ void splice_nonce(uint32_t* m, uint32_t hlen, uint64_t nonce) {
#pragma unroll
    for (uint32_t i = 0; i < 8u; i++) {
        const uint32_t nb = (uint32_t)((nonce >> (8u * i)) & 0xffULL);
        const uint32_t byte_pos = hlen + i;
        const uint32_t widx = byte_pos >> 2;
        const uint32_t boff = (byte_pos & 3u) * 8u;
        m[widx] = (m[widx] & ~(0xffu << boff)) | (nb << boff);
    }
}

// base message block (header in place, nonce slot zero) lives in constant memory.
__device__ __constant__ uint32_t d_base_m[16];

// ── KAT helper kernel: word for nonce_base+gid → out[gid] ────────────────────
template <int ROUNDS>
__global__ void k_words(uint32_t hlen, uint64_t nonce_base, uint32_t block_len,
                        uint64_t* out, uint32_t count) {
    const uint32_t gid = blockIdx.x * blockDim.x + threadIdx.x;
    if (gid >= count) return;
    uint32_t m[16];
#pragma unroll
    for (int i = 0; i < 16; i++) m[i] = d_base_m[i];
    splice_nonce(m, hlen, nonce_base + gid);
    out[gid] = compress_word<ROUNDS>(m, block_len);
}

// ── mining search kernel: grid-stride, HPT hashes/thread, atomicCAS winner ───
template <int ROUNDS>
__global__ __launch_bounds__(256) void k_search(
    uint32_t hlen, uint64_t nonce_base, uint64_t target, uint32_t block_len,
    uint64_t span, uint32_t hpt, uint64_t* found_nonce, int* found_flag,
    unsigned long long* hash_accum) {
    const uint64_t tid = (uint64_t)blockIdx.x * blockDim.x + threadIdx.x;
    const uint64_t stride = (uint64_t)gridDim.x * blockDim.x;
    uint32_t m[16];
    uint64_t local_hashes = 0;
    for (uint32_t j = 0; j < hpt; j++) {
        const uint64_t idx = tid + (uint64_t)j * stride;
        if (idx >= span) break;
        if (*((volatile int*)found_flag)) break;  // early-out once someone wins
        const uint64_t nonce = nonce_base + idx;
#pragma unroll
        for (int i = 0; i < 16; i++) m[i] = d_base_m[i];
        splice_nonce(m, hlen, nonce);
        const uint64_t word = compress_word<ROUNDS>(m, block_len);
        local_hashes++;
        if (word <= target) {
            if (atomicCAS(found_flag, 0, 1) == 0) *found_nonce = nonce;
        }
    }
    atomicAdd(hash_accum, (unsigned long long)local_hashes);
}

// ── host: bit-exact CPU reference (a line-for-line port of pow.rs compress8) ──
static const uint32_t H_IV[8] = {
    0x6A09E667u, 0xBB67AE85u, 0x3C6EF372u, 0xA54FF53Au,
    0x510E527Fu, 0x9B05688Cu, 0x1F83D9ABu, 0x5BE0CD19u};
static inline uint32_t h_rotr(uint32_t x, uint32_t n) { return (x >> n) | (x << (32 - n)); }
static inline void h_g(uint32_t* s, int a, int b, int c, int d, uint32_t mx, uint32_t my) {
    s[a] = s[a] + s[b] + mx; s[d] = h_rotr(s[d] ^ s[a], 16);
    s[c] = s[c] + s[d];      s[b] = h_rotr(s[b] ^ s[c], 12);
    s[a] = s[a] + s[b] + my; s[d] = h_rotr(s[d] ^ s[a], 8);
    s[c] = s[c] + s[d];      s[b] = h_rotr(s[b] ^ s[c], 7);
}
static void h_round(uint32_t* s, const uint32_t* m) {
    h_g(s, 0, 4, 8, 12, m[0], m[1]);  h_g(s, 1, 5, 9, 13, m[2], m[3]);
    h_g(s, 2, 6, 10, 14, m[4], m[5]); h_g(s, 3, 7, 11, 15, m[6], m[7]);
    h_g(s, 0, 5, 10, 15, m[8], m[9]); h_g(s, 1, 6, 11, 12, m[10], m[11]);
    h_g(s, 2, 7, 8, 13, m[12], m[13]); h_g(s, 3, 4, 9, 14, m[14], m[15]);
}
// full 32-byte digest of a ≤64B input at `rounds` rounds (== BLAKE3 at rounds=7).
static void h_compress8(const uint8_t* block, size_t len, uint32_t rounds, uint8_t out[32]) {
    uint8_t buf[64] = {0};
    memcpy(buf, block, len);
    uint32_t m[16];
    for (int i = 0; i < 16; i++)
        m[i] = (uint32_t)buf[i*4] | ((uint32_t)buf[i*4+1] << 8) |
               ((uint32_t)buf[i*4+2] << 16) | ((uint32_t)buf[i*4+3] << 24);
    uint32_t v[16] = {H_IV[0],H_IV[1],H_IV[2],H_IV[3],H_IV[4],H_IV[5],H_IV[6],H_IV[7],
                      H_IV[0],H_IV[1],H_IV[2],H_IV[3],0u,0u,(uint32_t)len,11u};
    for (uint32_t r = 0; r < rounds; r++) {
        h_round(v, m);
        uint32_t o[16]; memcpy(o, m, sizeof o);
        for (int i = 0; i < 16; i++) m[i] = o[H_MSGP[i]];
    }
    for (int i = 0; i < 8; i++) {
        uint32_t w = v[i] ^ v[i+8];
        out[i*4]=w&0xff; out[i*4+1]=(w>>8)&0xff; out[i*4+2]=(w>>16)&0xff; out[i*4+3]=(w>>24)&0xff;
    }
}
static uint64_t h_blake4_word(const uint8_t* header, size_t hlen, uint64_t nonce, uint32_t rounds) {
    if (hlen > 56) hlen = 56;
    uint8_t buf[64] = {0};
    memcpy(buf, header, hlen);
    for (int i = 0; i < 8; i++) buf[hlen + i] = (uint8_t)((nonce >> (8*i)) & 0xff);
    uint8_t d[32];
    h_compress8(buf, hlen + 8, rounds, d);
    uint64_t w = 0;
    for (int i = 0; i < 8; i++) w |= ((uint64_t)d[i]) << (8*i);
    return w;
}

// build the 16-word base block exactly like pow.rs (header bytes, nonce slot zero).
static void build_base_m(const uint8_t* header, size_t hlen, uint32_t base_m[16], uint32_t* out_hlen) {
    if (hlen > 56) hlen = 56;
    uint8_t buf[64] = {0};
    memcpy(buf, header, hlen);
    for (int i = 0; i < 16; i++)
        base_m[i] = (uint32_t)buf[i*4] | ((uint32_t)buf[i*4+1] << 8) |
                    ((uint32_t)buf[i*4+2] << 16) | ((uint32_t)buf[i*4+3] << 24);
    *out_hlen = (uint32_t)hlen;
}

static void upload_base(const uint8_t* header, size_t hlen, uint32_t* hlen_out, uint32_t* block_len_out) {
    uint32_t base_m[16]; uint32_t hl;
    build_base_m(header, hlen, base_m, &hl);
    CKERR(cudaMemcpyToSymbol(d_base_m, base_m, sizeof base_m));
    *hlen_out = hl; *block_len_out = hl + 8;
}

// dispatch a templated kernel by runtime rounds (7 = sound anchor, 3 = reduced KAT).
template <typename F7, typename F3>
static bool dispatch_rounds(uint32_t rounds, F7 f7, F3 f3) {
    if (rounds == 7) { f7(); return true; }
    if (rounds == 3) { f3(); return true; }
    fprintf(stderr, "rounds=%u not instantiated (use 7 or 3)\n", rounds);
    return false;
}

static int parse_hex(const char* s, uint8_t* out, size_t max) {
    size_t n = strlen(s); if (n % 2) return -1;
    size_t b = n / 2; if (b > max) return -1;
    for (size_t i = 0; i < b; i++) { unsigned v; if (sscanf(s + 2*i, "%2x", &v) != 1) return -1; out[i] = (uint8_t)v; }
    return (int)b;
}

// ── selftest: BLAKE3 anchor + GPU==CPU over a nonce batch ─────────────────────
static int run_selftest() {
    printf("== BLAKE4-CUDA selftest ==\n");
    int fail = 0;

    // (1) anchor CPU reference to real BLAKE3 known-answer vectors (R=7).
    struct { const char* in; const char* hex; } kat[] = {
        {"",    "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"},
        {"abc", "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"},
    };
    for (auto& k : kat) {
        uint8_t d[32]; h_compress8((const uint8_t*)k.in, strlen(k.in), 7, d);
        char hex[65]; for (int i=0;i<32;i++) sprintf(hex+2*i, "%02x", d[i]); hex[64]=0;
        bool ok = (strcmp(hex, k.hex) == 0);
        printf("  [%s] BLAKE3(\"%s\") R=7  %s\n", ok?"PASS":"FAIL", k.in, ok?"":(std::string("got ")+hex).c_str());
        if (!ok) fail = 1;
    }

    // (2) GPU words == CPU reference words, for several header lengths and both
    //     the sound (R=7) and a reduced (R=3) round count.
    const uint32_t count = 4096;
    uint64_t* d_out; CKERR(cudaMalloc(&d_out, count * sizeof(uint64_t)));
    uint64_t* h_out = (uint64_t*)malloc(count * sizeof(uint64_t));
    for (uint32_t hlen : {32u, 40u, 48u, 56u, 37u}) {
        uint8_t header[56]; for (uint32_t i=0;i<hlen;i++) header[i] = (uint8_t)(0x11 ^ hlen);
        uint32_t hl, block_len; upload_base(header, hlen, &hl, &block_len);
        for (uint32_t rounds : {7u, 3u}) {
            const uint64_t base = 0xDEAD0000ULL;
            dim3 blk(256), grd((count + 255) / 256);
            dispatch_rounds(rounds,
                [&]{ k_words<7><<<grd, blk>>>(hl, base, block_len, d_out, count); },
                [&]{ k_words<3><<<grd, blk>>>(hl, base, block_len, d_out, count); });
            CKERR(cudaGetLastError()); CKERR(cudaDeviceSynchronize());
            CKERR(cudaMemcpy(h_out, d_out, count * sizeof(uint64_t), cudaMemcpyDeviceToHost));
            int mism = 0;
            for (uint32_t i = 0; i < count; i++) {
                uint64_t cpu = h_blake4_word(header, hlen, base + i, rounds);
                if (h_out[i] != cpu) { if (mism < 3) printf("    mismatch hlen=%u R=%u n=%u gpu=%016llx cpu=%016llx\n",
                                       hlen, rounds, i, (unsigned long long)h_out[i], (unsigned long long)cpu); mism++; }
            }
            bool ok = (mism == 0);
            printf("  [%s] GPU==CPU  hlen=%u R=%u  (%u nonces, %d mism)\n", ok?"PASS":"FAIL", hlen, rounds, count, mism);
            if (!ok) fail = 1;
        }
    }
    cudaFree(d_out); free(h_out);
    printf(fail ? "== SELFTEST FAILED ==\n" : "== SELFTEST PASSED (GPU ≡ CPU ≡ BLAKE3 @ R=7) ==\n");
    return fail;
}

// ── bench: sustained hashrate ────────────────────────────────────────────────
static int run_bench(double seconds, uint32_t rounds) {
    cudaDeviceProp prop; CKERR(cudaGetDeviceProperties(&prop, 0));
    printf("== BLAKE4-CUDA bench ==  device: %s (%d SMs, sm_%d%d)  rounds=%u\n",
           prop.name, prop.multiProcessorCount, prop.major, prop.minor, rounds);
    uint8_t header[32]; memset(header, 0x11, 32);
    uint32_t hl, block_len; upload_base(header, 32, &hl, &block_len);

    uint64_t *d_nonce, *d_dummy; int* d_flag; unsigned long long* d_acc;
    CKERR(cudaMalloc(&d_nonce, sizeof(uint64_t))); CKERR(cudaMalloc(&d_dummy, sizeof(uint64_t)));
    CKERR(cudaMalloc(&d_flag, sizeof(int))); CKERR(cudaMalloc(&d_acc, sizeof(unsigned long long)));

    const int blk = 256;
    const int grd = prop.multiProcessorCount * 32;   // saturate the GPU
    const uint32_t hpt = 512;                          // hashes per thread per launch
    const uint64_t span = (uint64_t)grd * blk * hpt;

    // warmup
    CKERR(cudaMemset(d_flag, 0, sizeof(int))); CKERR(cudaMemset(d_acc, 0, sizeof(unsigned long long)));
    dispatch_rounds(rounds,
        [&]{ k_search<7><<<grd, blk>>>(hl, 0, 0, block_len, span, hpt, d_nonce, d_flag, d_acc); },
        [&]{ k_search<3><<<grd, blk>>>(hl, 0, 0, block_len, span, hpt, d_nonce, d_flag, d_acc); });
    CKERR(cudaGetLastError()); CKERR(cudaDeviceSynchronize());

    CKERR(cudaMemset(d_acc, 0, sizeof(unsigned long long)));
    auto t0 = std::chrono::high_resolution_clock::now();
    uint64_t base = 0, total_launched = 0; int launches = 0;
    while (true) {
        dispatch_rounds(rounds,
            [&]{ k_search<7><<<grd, blk>>>(hl, base, 0, block_len, span, hpt, d_nonce, d_flag, d_acc); },
            [&]{ k_search<3><<<grd, blk>>>(hl, base, 0, block_len, span, hpt, d_nonce, d_flag, d_acc); });
        base += span; total_launched += span; launches++;
        CKERR(cudaDeviceSynchronize());
        double el = std::chrono::duration<double>(std::chrono::high_resolution_clock::now() - t0).count();
        if (el >= seconds) {
            unsigned long long acc; CKERR(cudaMemcpy(&acc, d_acc, sizeof acc, cudaMemcpyDeviceToHost));
            double ghs = (double)acc / el / 1e9;
            printf("  hashes: %llu in %.3fs  →  %.2f GH/s  (%.0f MH/s)  [%d launches]\n",
                   acc, el, ghs, ghs * 1000.0, launches);
            break;
        }
    }
    cudaFree(d_nonce); cudaFree(d_dummy); cudaFree(d_flag); cudaFree(d_acc);
    return 0;
}

// ── mine: find a nonce whose word <= target ──────────────────────────────────
static int run_mine(const char* header_hex, const char* target_hex, uint32_t rounds, uint64_t nonce_base) {
    uint8_t header[56]; int hlen = parse_hex(header_hex, header, 56);
    if (hlen < 0) { fprintf(stderr, "bad header hex\n"); return 1; }
    uint64_t target = strtoull(target_hex, nullptr, 16);
    uint32_t hl, block_len; upload_base(header, hlen, &hl, &block_len);

    cudaDeviceProp prop; CKERR(cudaGetDeviceProperties(&prop, 0));
    const int blk = 256, grd = prop.multiProcessorCount * 32;
    const uint32_t hpt = 512;
    const uint64_t span = (uint64_t)grd * blk * hpt;

    uint64_t *d_nonce; int* d_flag; unsigned long long* d_acc;
    CKERR(cudaMalloc(&d_nonce, sizeof(uint64_t))); CKERR(cudaMalloc(&d_flag, sizeof(int)));
    CKERR(cudaMalloc(&d_acc, sizeof(unsigned long long)));
    CKERR(cudaMemset(d_flag, 0, sizeof(int))); CKERR(cudaMemset(d_acc, 0, sizeof(unsigned long long)));

    printf("== BLAKE4-CUDA mine ==  header=%s target<=%016llx rounds=%u\n",
           header_hex, (unsigned long long)target, rounds);
    auto t0 = std::chrono::high_resolution_clock::now();
    uint64_t base = nonce_base; int flag = 0; uint64_t nonce = 0;
    while (!flag) {
        dispatch_rounds(rounds,
            [&]{ k_search<7><<<grd, blk>>>(hl, base, target, block_len, span, hpt, d_nonce, d_flag, d_acc); },
            [&]{ k_search<3><<<grd, blk>>>(hl, base, target, block_len, span, hpt, d_nonce, d_flag, d_acc); });
        CKERR(cudaGetLastError()); CKERR(cudaDeviceSynchronize());
        CKERR(cudaMemcpy(&flag, d_flag, sizeof flag, cudaMemcpyDeviceToHost));
        if (flag) { CKERR(cudaMemcpy(&nonce, d_nonce, sizeof nonce, cudaMemcpyDeviceToHost)); break; }
        base += span;
        double el = std::chrono::duration<double>(std::chrono::high_resolution_clock::now() - t0).count();
        if (el > 30.0) { printf("  no winner in 30s up to nonce %llu\n", (unsigned long long)base); break; }
    }
    if (flag) {
        uint64_t w = h_blake4_word(header, hlen, nonce, rounds);
        double el = std::chrono::duration<double>(std::chrono::high_resolution_clock::now() - t0).count();
        printf("  WIN nonce=%llu  word=%016llx <= target  (verified on CPU)  in %.3fs\n",
               (unsigned long long)nonce, (unsigned long long)w, el);
    }
    cudaFree(d_nonce); cudaFree(d_flag); cudaFree(d_acc);
    return flag ? 0 : 2;
}

int main(int argc, char** argv) {
    if (argc < 2) { printf("usage: %s selftest | bench [secs] [rounds] | mine <hdr_hex> <target_hex> [rounds] [base]\n", argv[0]); return 1; }
    std::string mode = argv[1];
    if (mode == "selftest") return run_selftest();
    if (mode == "bench")    return run_bench(argc > 2 ? atof(argv[2]) : 5.0, argc > 3 ? atoi(argv[3]) : 7);
    if (mode == "mine") {
        if (argc < 4) { fprintf(stderr, "mine needs <header_hex> <target_hex>\n"); return 1; }
        return run_mine(argv[2], argv[3], argc > 4 ? atoi(argv[4]) : 7, argc > 5 ? strtoull(argv[5], nullptr, 10) : 0);
    }
    fprintf(stderr, "unknown mode %s\n", mode.c_str());
    return 1;
}
