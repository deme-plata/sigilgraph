inline ulong mers31(ulong x){ x=(x&0x7fffffffUL)+(x>>31); x=(x&0x7fffffffUL)+(x>>31); return x>=2147483647UL? x-2147483647UL : x; }
__kernel void ajtai_commit_batch(__global const ulong* A,__global const ulong* W,__global ulong* C,const uint m,const uint n,const ulong Q){
    ulong gid=(ulong)get_global_id(0); ulong wi=gid/(ulong)m; uint i=(uint)(gid%(ulong)m);
    __global const ulong* row=A+(ulong)i*(ulong)n; __global const ulong* w=W+wi*(ulong)n;
    ulong acc=0; for(uint j=0;j<n;j++) acc+=mers31(row[j]*w[j]); C[wi*(ulong)m+(ulong)i]=mers31(acc);
}
__kernel void rho_combine(__global const ulong* MAT,__global const ulong* RHO,__global ulong* OUT,const uint rows,const uint cols){
    uint k=get_global_id(0); ulong acc=0;
    for(uint i=0;i<rows;i++) acc+=mers31(MAT[(ulong)i*cols+k]*RHO[i]);
    OUT[k]=mers31(acc);
}
