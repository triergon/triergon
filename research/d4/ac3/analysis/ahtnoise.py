import sys, os, subprocess, json, numpy as np
from math import pi
from numpy import i0
exec(open(sys.argv[5]).read().split('REM = ')[0])  # reuse loading code from noise.py
REM = [13, 25, 37, 61, 253]
Mx=np.array([[np.sqrt(2)*(1/np.sqrt(2) if j==0 else 1)*np.cos(j*(2*m+1)*np.pi/12) for j in range(6)] for m in range(6)])
xs=[]
for f in range(1, nb//6 - 1):
    j=J[f*6]
    if not any(j['aht'][:2]): continue
    for c in [0,1]:
        if not j['aht'][c]: continue
        for i in range(j['end'][c]):
            if j['bap'][c][i]!=0: continue
            Cs=[]
            ok=True
            for m in range(6):
                jj=J[f*6+m]; b=f*6+m
                remat=False
                if jj['nremat'] and i>=13:
                    bnd=max(x for x in range(4) if REM[x]<=i); remat=bnd<jj['nremat'] and jj['remat'][bnd]
                v = ((X[0][b][i]+X[1][b][i])/2 if c==0 else (X[0][b][i]-X[1][b][i])/2) if remat else X[c][b][i]
                Cs.append(v/jj['dyn'][0]*2**j['exps'][c][i])
            xs.append(Mx.T@np.array(Cs)/6)
xs=np.array(xs)
print('n',len(xs))
for jx in range(6):
    v=xs[:,jx]; print('X[%d] std %.3f q: %s'%(jx, v.std(), np.round(np.percentile(v,[0,1,5,25,50,75,95,99,100]),3)))
