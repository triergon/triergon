static void fold(uint64_t *digest,uint64_t n){*digest=*digest*31+n;}
/* Same exact eight-byte polynomial, grouped into bounded 32-bit dots.
 * Four-byte maximum is 7,849,920, so these intermediate sums never wrap. */
static uint32_t fold_four(const uint8_t *b){
    return (uint32_t)b[0]*UINT32_C(29791)+(uint32_t)b[1]*UINT32_C(961)
        +(uint32_t)b[2]*UINT32_C(31)+(uint32_t)b[3];
}
static uint64_t fold_eight(const uint8_t *b){
    return (uint64_t)fold_four(b)*UINT64_C(923521)+(uint64_t)fold_four(b+4);
}
static void fold_bytes(uint64_t *digest,const uint8_t *bytes,size_t n){
    /* Same ordered checksum and polynomial as the Rust worker. All payload and
     * metadata bytes remain consumed inside each measured operation. */
    size_t i=0;
    if(n>=256){
        const uint64_t step=UINT64_C(3671467063254694913);
        size_t exponent=n/32;uint64_t power=1,base=step,lanes[4]={0,0,0,0};
        while(exponent){if(exponent&1)power*=base;base*=base;exponent>>=1;}
        for(;n-i>=32;i+=32)for(unsigned j=0;j<4;j++)lanes[j]=lanes[j]*step+fold_eight(bytes+i+j*8);
        *digest=*digest*power+lanes[0]*UINT64_C(13349005091172011265)
            +lanes[1]*UINT64_C(12662688596514954753)+lanes[2]*UINT64_C(852891037441)+lanes[3];
    }
    for(;n-i>=8;i+=8)*digest=*digest*UINT64_C(852891037441)+fold_eight(bytes+i);
    for(;i<n;i++)fold(digest,bytes[i]);
}
