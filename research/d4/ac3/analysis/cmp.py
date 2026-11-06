import sys, numpy as np
a = np.fromfile(sys.argv[1], dtype='<f4'); b = np.fromfile(sys.argv[2], dtype='<f4'); ch=int(sys.argv[3])
n=min(len(a),len(b))
print("len", len(a)//ch, len(b)//ch, "frames", len(a)/ch/1536, len(b)/ch/1536)
a=a[:n].reshape(-1,ch); b=b[:n].reshape(-1,ch)
d=a-b
for c in range(ch):
    rms=np.sqrt(np.mean(d[:,c].astype(np.float64)**2)); mx=np.max(np.abs(d[:,c]))
    ok = rms<=8.8e-6 and mx<=6.1035e-5
    first = np.argmax(np.abs(d[:,c])>6.1035e-5) if mx>6.1035e-5 else -1
    print(f"ch{c} rms={rms:.3e} max={mx:.3e} {'PASS' if ok else 'FAIL'} first_bad_sample={first} (frame {first//1536 if first>=0 else -1}) refpeak={np.max(np.abs(b[:,c])):.3f}")
