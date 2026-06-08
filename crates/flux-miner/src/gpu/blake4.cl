// blake4.cl — BLAKE4 proof-of-work search kernel (Lane A on the GPU).
//
// One work-item = one nonce. Each computes BLAKE4(header‖nonce) at `rounds`
// rounds and, if the first-8-byte word is <= target, claims the win. This is a
// BYTE-FOR-BYTE port of flux_miner::pow::compress8 / blake4_word — same IV,
// message schedule, G mix, flags (CHUNK_START|CHUNK_END|ROOT = 11), counter 0,
// single ≤64-byte block. KAT requirement: for the same (header,nonce,rounds=7)
// this kernel MUST produce the same word as pow::blake4_word (validate on a GPU).

__constant uint IV[8] = {
    0x6A09E667u, 0xBB67AE85u, 0x3C6EF372u, 0xA54FF53Au,
    0x510E527Fu, 0x9B05688Cu, 0x1F83D9ABu, 0x5BE0CD19u
};
__constant uchar MSGP[16] = {2,6,3,10,7,0,4,13,1,11,12,5,9,14,15,8};

inline uint rotr(uint x, uint n) { return (x >> n) | (x << (32u - n)); }

inline void Gmix(uint *v, int a, int b, int c, int d, uint mx, uint my) {
    v[a] = v[a] + v[b] + mx; v[d] = rotr(v[d] ^ v[a], 16u);
    v[c] = v[c] + v[d];      v[b] = rotr(v[b] ^ v[c], 12u);
    v[a] = v[a] + v[b] + my; v[d] = rotr(v[d] ^ v[a], 8u);
    v[c] = v[c] + v[d];      v[b] = rotr(v[b] ^ v[c], 7u);
}

inline void roundf(uint *v, const uint *m) {
    Gmix(v, 0, 4, 8, 12, m[0], m[1]);
    Gmix(v, 1, 5, 9, 13, m[2], m[3]);
    Gmix(v, 2, 6, 10, 14, m[4], m[5]);
    Gmix(v, 3, 7, 11, 15, m[6], m[7]);
    Gmix(v, 0, 5, 10, 15, m[8], m[9]);
    Gmix(v, 1, 6, 11, 12, m[10], m[11]);
    Gmix(v, 2, 7, 8, 13, m[12], m[13]);
    Gmix(v, 3, 4, 9, 14, m[14], m[15]);
}

inline void permute(uint *m) {
    uint o[16];
    for (int i = 0; i < 16; i++) o[i] = m[i];
    for (int i = 0; i < 16; i++) m[i] = o[MSGP[i]];
}

// base_m: the 64-byte block as 16 LE words, header bytes in place, nonce slot zero.
// Each work-item splices its own nonce at byte offset hlen, then compresses.
__kernel void blake4_search(
    __global const uint *base_m, // [16] header‖0(nonce)‖pad as LE words
    const uint hlen,             // header length in bytes (nonce at hlen..hlen+8)
    const ulong nonce_base,
    const ulong target,          // first-8-byte word must be <= target
    const uint rounds,
    const uint block_len,        // hlen + 8
    __global ulong *found_nonce,
    __global int *found_flag)
{
    const ulong nonce = nonce_base + (ulong)get_global_id(0);

    uint m[16];
    for (int i = 0; i < 16; i++) m[i] = base_m[i];

    // splice the 8 little-endian nonce bytes at byte offset hlen
    for (uint i = 0; i < 8u; i++) {
        const uchar nb = (uchar)((nonce >> (8u * i)) & 0xffUL);
        const uint byte_pos = hlen + i;
        const uint widx = byte_pos >> 2;
        const uint boff = (byte_pos & 3u) * 8u;
        m[widx] = (m[widx] & ~(0xffu << boff)) | ((uint)nb << boff);
    }

    uint v[16];
    for (int i = 0; i < 8; i++) v[i] = IV[i];
    v[8] = IV[0]; v[9] = IV[1]; v[10] = IV[2]; v[11] = IV[3];
    v[12] = 0u; v[13] = 0u; v[14] = block_len; v[15] = 11u; // START|END|ROOT

    for (uint r = 0; r < rounds; r++) { roundf(v, m); permute(m); }

    const uint w0 = v[0] ^ v[8];
    const uint w1 = v[1] ^ v[9];
    const ulong word = ((ulong)w0) | (((ulong)w1) << 32);

    if (word <= target) {
        if (atomic_cmpxchg(found_flag, 0, 1) == 0) {
            *found_nonce = nonce;
        }
    }
}

// blake4_words — KAT helper: write the BLAKE4 word for nonce_base+gid to out[gid].
// Lets the host compare GPU output against pow::blake4_word for identical inputs
// (the on-hardware known-answer test). Same compression as blake4_search.
__kernel void blake4_words(
    __global const uint *base_m,
    const uint hlen,
    const ulong nonce_base,
    const uint rounds,
    const uint block_len,
    __global ulong *out)
{
    const ulong nonce = nonce_base + (ulong)get_global_id(0);

    uint m[16];
    for (int i = 0; i < 16; i++) m[i] = base_m[i];
    for (uint i = 0; i < 8u; i++) {
        const uchar nb = (uchar)((nonce >> (8u * i)) & 0xffUL);
        const uint byte_pos = hlen + i;
        const uint widx = byte_pos >> 2;
        const uint boff = (byte_pos & 3u) * 8u;
        m[widx] = (m[widx] & ~(0xffu << boff)) | ((uint)nb << boff);
    }

    uint v[16];
    for (int i = 0; i < 8; i++) v[i] = IV[i];
    v[8] = IV[0]; v[9] = IV[1]; v[10] = IV[2]; v[11] = IV[3];
    v[12] = 0u; v[13] = 0u; v[14] = block_len; v[15] = 11u;

    for (uint r = 0; r < rounds; r++) { roundf(v, m); permute(m); }

    const uint w0 = v[0] ^ v[8];
    const uint w1 = v[1] ^ v[9];
    out[get_global_id(0)] = ((ulong)w0) | (((ulong)w1) << 32);
}
