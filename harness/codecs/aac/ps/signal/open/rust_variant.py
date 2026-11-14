"""Probe instrument of the open-case probes o20..o32: patches a scratch copy of this
decoder (a separate checkout, never the one under test) so that the behaviours below can
be switched at run time by the environment variable PSV (comma-separated key=value; an
absent key keeps the decoder's behaviour). Patch a fresh checkout's
crates/codec-aac/src/sbr/ps.rs with this script, then build its decode_aac example
(cargo build --release -p triergon-codec-aac --example decode_aac) and pass that binary
to the probes.

Switches (sbr/ps.rs unless noted):
  hist=raw|conv|zero    IPD/OPD smoothing history at a change frame: read as stored,
                         converted like coded values (truncated index means), or zero
  imkeep=store|start     imaginary parts above the IPD bands at a change frame: the stored
                         row, or the converted start's row
  cornerhist=raw|zero|conv  the same history at a frame that runs in another layout than
                         the last frame run without a registered change
  cornerphased=own|last  the bands that take IPD/OPD phases in such a frame: those of its
                         own layout, or those of the layout of the last frame run
  cornerstartim=raw|conv|zero  the imaginary part of such a frame's start matrix: read as
                         stored, converted from the last frame run's layout, or zero
  abovephase=cos|none    bands at or above the IPD band count: real part turned by the
                         smoothed phase (as the decoder does) or left unturned
  histadv=all|ipd        the smoothing history advances in every band or in the IPD bands
  nosbrhi=keep|zero      a frame without SBR data: the PS output of QMF bands 32..63 kept
                         or set to zero in both channels (sbr/mod.rs)
  nosbrprev=keep|zero    the same frame: the previous frame's high band reaching into it
                         (an envelope ending after the frame border) kept or dropped, in
                         the QMF matrix itself, with or without PS (sbr/mod.rs)

usage: rust_variant.py PATH/TO/sbr/ps.rs   (sbr/mod.rs next to it is patched too)"""
import sys

p = sys.argv[1]
s = open(p).read()
assert 'fn psv(' not in s, 'already patched'

s = s.replace("""use std::sync::OnceLock;""", """use std::sync::OnceLock;

/// Probe switch (scratch copy only).
pub(crate) fn psv(key: &str) -> String {
    static V: OnceLock<String> = OnceLock::new();
    let v = V.get_or_init(|| std::env::var("PSV").unwrap_or_default());
    v.split(',')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}""", 1)

old = """        if self.changed() {
            let old = self.big_before == Some(true);
            convert(&mut re0, old);
            convert(&mut im0, old);
        }"""
new = """        if self.changed() {
            let old = self.big_before == Some(true);
            convert(&mut re0, old);
            convert(&mut im0, old);
            match psv("hist").as_str() {
                "zero" => self.phase_history = [[[0; MAX_PAR]; 2]; 2],
                "conv" => {
                    let nb = if old { 34 } else { 20 };
                    for h in 0..2 {
                        for k in 0..2 {
                            let src = self.phase_history[h][k];
                            to_config(&src[..], nb, f.big, &mut self.phase_history[h][k]);
                        }
                    }
                }
                _ => {}
            }
        }
        let change_start_im = im0;
        let changed_now = self.changed();"""
assert old in s
s = s.replace(old, new, 1)

old = """                    im1[b][i] = if b < phased {
                        (f64::from(v) * s) as f32
                    } else {
                        self.store_im[e][b][i]
                    };"""
new = """                    im1[b][i] = if b < phased {
                        (f64::from(v) * s) as f32
                    } else if changed_now && e == 0 && psv("imkeep") == "start" {
                        change_start_im[b][i]
                    } else {
                        self.store_im[e][b][i]
                    };"""
assert old in s
s = s.replace(old, new, 1)
old = """        let change_start_im = im0;"""
new = """        thread_local!(static LAST_RUN: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) });
        let corner = LAST_RUN.with(|c| c.get()).is_some_and(|b| b != f.big) && !self.changed();
        LAST_RUN.with(|c| c.set(Some(f.big)));
        if corner {
            match psv("cornerhist").as_str() {
                "zero" => self.phase_history = [[[0; MAX_PAR]; 2]; 2],
                "conv" => {
                    let nb = if f.big { 20 } else { 34 };
                    for h in 0..2 {
                        for k in 0..2 {
                            let src = self.phase_history[h][k];
                            to_config(&src[..], nb, f.big, &mut self.phase_history[h][k]);
                        }
                    }
                }
                _ => {}
            }
        }
        let change_start_im = im0;"""
assert old in s
s = s.replace(old, new, 1)
old = """        let change_start_im = im0;"""
new = """        match psv("cornerstartim").as_str() {
            "zero" if corner => im0 = [[0.0; 4]; MAX_PAR],
            "conv" if corner => convert(&mut im0, !f.big),
            _ => {}
        }
        self.start_im = im0;
        let change_start_im = im0;"""
assert old in s
s = s.replace(old, new, 1)
old = """            let phased = ipd_bands(if f.big { 34 } else { 20 });"""
new = """            let phased = ipd_bands(if f.big ^ (corner && psv("cornerphased") == "last") { 34 } else { 20 });"""
assert old in s
s = s.replace(old, new, 1)
old = """                    let (c, s) = if i < 2 { pl[b] } else { pr[b] };"""
new = """                    let (c, s) = if i < 2 { pl[b] } else { pr[b] };
                    let c = if b >= phased && psv("abovephase") == "none" { 1.0 } else { c };"""
assert old in s
s = s.replace(old, new, 1)
old = """            self.phase_history[1][k] = self.phase_history[0][k];
            self.phase_history[0][k] = cur[k];"""
new = """            if psv("histadv") == "ipd" {
                let n = ipd_bands(if big { 34 } else { 20 });
                for b in 0..n {
                    self.phase_history[1][k][b] = self.phase_history[0][k][b];
                    self.phase_history[0][k][b] = cur[k][b];
                }
            } else {
                self.phase_history[1][k] = self.phase_history[0][k];
                self.phase_history[0][k] = cur[k];
            }"""
assert old in s
s = s.replace(old, new, 1)
open(p, 'w').write(s)
print('patched', p)

# sbr/mod.rs: the frame-without-SBR switch
q = p.replace('sbr/ps.rs', 'sbr/mod.rs')
t = open(q).read()
old = """                if self.ps_seen {
                    self.ps.apply(&mut self.x, &mut self.x_r);"""
new = """                if self.ps_seen {
                    self.ps.apply(&mut self.x, &mut self.x_r);
                    if !ready && ps::psv("nosbrhi") == "zero" {
                        for i in 0..32 {
                            for k in 32..64 {
                                self.x[i][k] = Cpx::default();
                                self.x_r[i][k] = Cpx::default();
                            }
                        }
                    }"""
assert old in t
t = t.replace(old, new, 1)
old = """                if i < i_temp {
                    row[hf.clone()].copy_from_slice(&ch.y_prev[i][hf]);"""
new = """                if i < i_temp && !(!ready && ps::psv("nosbrprev") == "zero") {
                    row[hf.clone()].copy_from_slice(&ch.y_prev[i][hf]);"""
assert old in t
t = t.replace(old, new, 1)
open(q, 'w').write(t)
print('patched', q)
