"""Independent zlib oracle and malformed-stream/resource regressions."""
import json,random,subprocess,tempfile,unittest,zlib
from pathlib import Path
from .runner import ROOT
WORKER=ROOT/'target/release/media-worker'

class ZlibTests(unittest.TestCase):
    def invoke(self,data,limit=1<<20,ratio=1024,work=1<<26):
        with tempfile.TemporaryDirectory() as directory:
            p=Path(directory)/'stream.zlib';p.write_bytes(data)
            return subprocess.run([WORKER,'inflate',p,str(limit),str(ratio),str(work)],capture_output=True,text=True,timeout=5)
    def test_stored_fixed_dynamic_windows_blocks_and_overlap(self):
        rng=random.Random(1951)
        inputs=[b'',b'a',bytes(range(256))*256,b'abcde'*6000,bytes(rng.randrange(256) for _ in range(20000))]
        inputs += [bytes(rng.choices(range(256),weights=[1/(1+i) for i in range(256)],k=n)) for n in [8,129,4096,32769]]
        for window in [9,12,15]:
            for level,strategy in [(0,zlib.Z_DEFAULT_STRATEGY),(6,zlib.Z_FIXED),(6,zlib.Z_DEFAULT_STRATEGY),(9,zlib.Z_HUFFMAN_ONLY)]:
                for original in inputs:
                    encoder=zlib.compressobj(level,zlib.DEFLATED,window,8,strategy)
                    # Flush a nonfinal block, retaining cross-block history.
                    half=len(original)//2
                    encoded=encoder.compress(original[:half])+encoder.flush(zlib.Z_SYNC_FLUSH)+encoder.compress(original[half:])+encoder.flush()
                    result=self.invoke(encoded)
                    self.assertEqual(result.returncode,0,result.stderr)
                    self.assertEqual(bytes.fromhex(json.loads(result.stdout)),original)
    def test_bad_header_checksum_dictionary_truncation_and_limits(self):
        data=zlib.compress(b'abcde'*3000)
        for cut in range(len(data)):
            self.assertNotEqual(self.invoke(data[:cut]).returncode,0)
        for encoded in [b'\x78\x00'+data[2:],data[:-1]+bytes([data[-1]^1]),data+b'extra',b'\x78\x20'+data[2:]]:
            self.assertNotEqual(self.invoke(encoded).returncode,0)
        for args in [(100,1024,1<<26),(1<<20,1,1<<26),(1<<20,1024,1)]:
            result=self.invoke(data,*args)
            self.assertNotEqual(result.returncode,0);self.assertIn('resource limit',result.stderr)
    def test_mutations_never_panic_or_hang(self):
        rng=random.Random(1950);valid=zlib.compress(bytes(range(256))*4)
        for _ in range(160):
            data=bytearray(valid);at=rng.randrange(len(data));data[at]^=1<<rng.randrange(8)
            result=self.invoke(data,65536,1024,65536)
            self.assertNotIn('panicked',result.stderr)
            self.assertNotIn('signal',result.stderr)
            if result.returncode==0:
                self.assertEqual(bytes.fromhex(json.loads(result.stdout)),zlib.decompress(data))
