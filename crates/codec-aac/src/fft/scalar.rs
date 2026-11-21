//! The scalar forms of the DCT-IV's contiguous passes: the pre-twiddle, the radix-4
//! stages after the first, the post-twiddle and the last radix-2 stage fused with it.

/// One radix-4 stage with quarters of `h` points: the butterflies of each group of `4h`
/// points.
pub(super) fn stage(re: &mut [f32], im: &mut [f32], h: usize, w: &[Vec<f32>; 6]) {
    if h == 4 {
        // one 4-lane vector per quarter: fixed-size blocks so the lanes vectorise
        let w: [[f32; 4]; 6] = std::array::from_fn(|t| std::array::from_fn(|k| w[t][k]));
        let (re, im) = (re.as_chunks_mut::<16>().0, im.as_chunks_mut::<16>().0);
        for (br, bi) in re.iter_mut().zip(im) {
            for k in 0..4 {
                radix4(
                    br,
                    bi,
                    [k, 4 + k, 8 + k, 12 + k],
                    [w[0][k], w[1][k], w[2][k], w[3][k], w[4][k], w[5][k]],
                );
            }
        }
        return;
    }
    for (br, bi) in re.chunks_exact_mut(4 * h).zip(im.chunks_exact_mut(4 * h)) {
        let (r01, r23) = br.split_at_mut(2 * h);
        let (r0, r1) = r01.split_at_mut(h);
        let (r2, r3) = r23.split_at_mut(h);
        let (i01, i23) = bi.split_at_mut(2 * h);
        let (i0, i1) = i01.split_at_mut(h);
        let (i2, i3) = i23.split_at_mut(h);
        radix4_block(r0, r1, r2, r3, i0, i1, i2, i3, w);
    }
}

/// h radix-4 butterflies between the quarters `r`/`i` (each of length h). A separate,
/// non-inlined function: its `&mut` arguments are known not to alias, so the loop
/// vectorises without runtime overlap checks.
#[inline(never)]
// separate slice arguments are the point: each gets its own no-alias guarantee
#[allow(clippy::too_many_arguments)]
fn radix4_block(
    r0: &mut [f32],
    r1: &mut [f32],
    r2: &mut [f32],
    r3: &mut [f32],
    i0: &mut [f32],
    i1: &mut [f32],
    i2: &mut [f32],
    i3: &mut [f32],
    w: &[Vec<f32>; 6],
) {
    let h = r0.len();
    let (r1, r2, r3, i0, i1, i2, i3) = (
        &mut r1[..h],
        &mut r2[..h],
        &mut r3[..h],
        &mut i0[..h],
        &mut i1[..h],
        &mut i2[..h],
        &mut i3[..h],
    );
    let [w1r, w1i, w2r, w2i, w3r, w3i] = w;
    let (w1r, w1i, w2r, w2i, w3r, w3i) = (
        &w1r[..h],
        &w1i[..h],
        &w2r[..h],
        &w2i[..h],
        &w3r[..h],
        &w3i[..h],
    );
    for k in 0..h {
        let (t0r, t0i) = (r0[k], i0[k]);
        let t1r = r1[k] * w2r[k] - i1[k] * w2i[k];
        let t1i = r1[k] * w2i[k] + i1[k] * w2r[k];
        let t2r = r2[k] * w1r[k] - i2[k] * w1i[k];
        let t2i = r2[k] * w1i[k] + i2[k] * w1r[k];
        let t3r = r3[k] * w3r[k] - i3[k] * w3i[k];
        let t3i = r3[k] * w3i[k] + i3[k] * w3r[k];
        let (s01r, s01i, d01r, d01i) = (t0r + t1r, t0i + t1i, t0r - t1r, t0i - t1i);
        let (s23r, s23i, d23r, d23i) = (t2r + t3r, t2i + t3i, t2r - t3r, t2i - t3i);
        r0[k] = s01r + s23r;
        i0[k] = s01i + s23i;
        r2[k] = s01r - s23r;
        i2[k] = s01i - s23i;
        r1[k] = d01r + d23i;
        i1[k] = d01i - d23r;
        r3[k] = d01r - d23i;
        i3[k] = d01i + d23r;
    }
}

/// One radix-4 butterfly on positions `p` (quarters 0..3) with twiddles [W^k, W^2k, W^3k]
/// as (re, im) pairs. Same arithmetic as the general stage loop.
#[inline(always)]
fn radix4(r: &mut [f32; 16], i: &mut [f32; 16], p: [usize; 4], w: [f32; 6]) {
    let (t0r, t0i) = (r[p[0]], i[p[0]]);
    let t1r = r[p[1]] * w[2] - i[p[1]] * w[3];
    let t1i = r[p[1]] * w[3] + i[p[1]] * w[2];
    let t2r = r[p[2]] * w[0] - i[p[2]] * w[1];
    let t2i = r[p[2]] * w[1] + i[p[2]] * w[0];
    let t3r = r[p[3]] * w[4] - i[p[3]] * w[5];
    let t3i = r[p[3]] * w[5] + i[p[3]] * w[4];
    let (s01r, s01i, d01r, d01i) = (t0r + t1r, t0i + t1i, t0r - t1r, t0i - t1i);
    let (s23r, s23i, d23r, d23i) = (t2r + t3r, t2i + t3i, t2r - t3r, t2i - t3i);
    r[p[0]] = s01r + s23r;
    i[p[0]] = s01i + s23i;
    r[p[2]] = s01r - s23r;
    i[p[2]] = s01i - s23i;
    r[p[1]] = d01r + d23i;
    i[p[1]] = d01i - d23r;
    r[p[3]] = d01r - d23i;
    i[p[3]] = d01i + d23r;
}

/// The slice's first `n` elements as blocks of four.
#[inline(always)]
fn blocks(s: &[f32], n: usize) -> &[[f32; 4]] {
    s[..n].as_chunks::<4>().0
}

/// The slice's first `n` elements as mutable blocks of four.
#[inline(always)]
fn blocks_mut(s: &mut [f32], n: usize) -> &mut [[f32; 4]] {
    s[..n].as_chunks_mut::<4>().0
}

// The twiddle loops below run over blocks of four outputs, so that on targets without
// post-incrementing float loads each stream advances once a block and the loads use
// immediate offsets; a block is also one vector on targets with float vectors.

/// DCT-IV pre-twiddle: (x[2n] + i x[m - 1 - 2n]) * pre[n].
#[inline(never)]
pub(super) fn pre(x: &[f32], pr: &[f32], pi: &[f32], tr: &mut [f32], ti: &mut [f32]) {
    let h = tr.len();
    let (pr, pi, tr, ti) = (
        blocks(pr, h),
        blocks(pi, h),
        blocks_mut(tr, h),
        blocks_mut(ti, h),
    );
    // x[2n] for n = 4b + j is xs[b][2j], and x[2h - 1 - 2n] is xs[h/4 - 1 - b][7 - 2j]
    let xs = x[..2 * h].as_chunks::<8>().0;
    for (b, (((tr, ti), pr), pi)) in tr.iter_mut().zip(ti).zip(pr).zip(pi).enumerate() {
        let (fwd, rev) = (&xs[b], &xs[h / 4 - 1 - b]);
        for j in 0..4 {
            let (x0, x1) = (fwd[2 * j], rev[7 - 2 * j]);
            tr[j] = x0 * pr[j] - x1 * pi[j];
            ti[j] = x0 * pi[j] + x1 * pr[j];
        }
    }
}

/// DCT-IV post-twiddle: ye = Re(z q), yo = -Im(z q).
#[inline(never)]
pub(super) fn post(re: &[f32], im: &[f32], qr: &[f32], qi: &[f32], ye: &mut [f32], yo: &mut [f32]) {
    let h = re.len();
    let (re, im, qr, qi) = (blocks(re, h), blocks(im, h), blocks(qr, h), blocks(qi, h));
    let (ye, yo) = (blocks_mut(ye, h), blocks_mut(yo, h));
    for (b, (ye, yo)) in ye.iter_mut().zip(yo).enumerate() {
        let (re, im, qr, qi) = (&re[b], &im[b], &qr[b], &qi[b]);
        for j in 0..4 {
            ye[j] = re[j] * qr[j] - im[j] * qi[j];
            yo[j] = -(re[j] * qi[j] + im[j] * qr[j]);
        }
    }
}

/// Last radix-2 FFT stage (z = lo +/- w hi) fused with the DCT-IV post-twiddle of both
/// halves. Non-inlined so the non-aliasing arguments let the loop vectorise.
#[inline(never)]
// separate slice arguments are the point: each gets its own no-alias guarantee
#[allow(clippy::too_many_arguments)]
pub(super) fn last_post(
    re0: &[f32],
    re1: &[f32],
    im0: &[f32],
    im1: &[f32],
    wr: &[f32],
    wi: &[f32],
    qr: &[f32],
    qi: &[f32],
    ye0: &mut [f32],
    ye1: &mut [f32],
    yo0: &mut [f32],
    yo1: &mut [f32],
) {
    let q = re0.len();
    let (re0, re1, im0, im1) = (
        blocks(re0, q),
        blocks(re1, q),
        blocks(im0, q),
        blocks(im1, q),
    );
    let (wr, wi) = (blocks(wr, q), blocks(wi, q));
    let (qr0, qr1) = (blocks(qr, q), blocks(&qr[q..], q));
    let (qi0, qi1) = (blocks(qi, q), blocks(&qi[q..], q));
    let (ye0, ye1) = (blocks_mut(ye0, q), blocks_mut(ye1, q));
    let (yo0, yo1) = (blocks_mut(yo0, q), blocks_mut(yo1, q));
    for b in 0..q / 4 {
        let (re0, re1, im0, im1, wr, wi) = (&re0[b], &re1[b], &im0[b], &im1[b], &wr[b], &wi[b]);
        let (qr0, qr1, qi0, qi1) = (&qr0[b], &qr1[b], &qi0[b], &qi1[b]);
        for j in 0..4 {
            let tr = re1[j] * wr[j] - im1[j] * wi[j];
            let ti = re1[j] * wi[j] + im1[j] * wr[j];
            let (zr, zi) = (re0[j] + tr, im0[j] + ti);
            ye0[b][j] = zr * qr0[j] - zi * qi0[j];
            yo0[b][j] = -(zr * qi0[j] + zi * qr0[j]);
            let (zr, zi) = (re0[j] - tr, im0[j] - ti);
            ye1[b][j] = zr * qr1[j] - zi * qi1[j];
            yo1[b][j] = -(zr * qi1[j] + zi * qr1[j]);
        }
    }
}
