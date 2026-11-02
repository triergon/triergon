"""Independent scalar proof of the checksum work in both timing workers."""
import json
import subprocess
import unittest
from .runner import ROOT

MASK = (1 << 64) - 1


def scalar_trace_checksum(trace):
    value = 0
    def field(n):
        nonlocal value
        value = (value * 31 + (int(n) & MASK)) & MASK
    def payload(data):
        for byte in data:
            field(byte)
    for track in trace['tracks']:
        payload(track['codec'].encode())
        payload(bytes.fromhex(track['private']))
        payload(track['language'].encode())
        payload(track['name'].encode())
        for key in ['kind', 'width', 'height', 'channels', 'sample_rate', 'default', 'forced']:
            field(track[key])
        for n in track.get("colour",[]):field(n)
    for packet in trace['packets']:
        payload(bytes.fromhex(packet['payload']))
        for key in ['track', 'pts_ns', 'dts_ns', 'duration_ns', 'key', 'trim_start_samples', 'trim_end_samples']:
            n = packet[key]
            field(n if n is not None else 0 if key == 'duration_ns' else -(1 << 63))
    return value


class ChecksumTests(unittest.TestCase):
    def test_both_workers_match_the_original_ordered_checksum(self):
        workers = [ROOT / 'target/release/media-worker', ROOT / 'target/harness/ffmpeg-worker']
        for directory in ['fixtures', 'header-fixtures', 'zlib-fixtures', 'seek-fixtures']:
            for path in sorted((ROOT / '.research' / directory).glob('*.mkv')):
                for worker in workers:
                    trace = json.loads(subprocess.check_output([worker, 'trace', path], stderr=subprocess.PIPE))
                    measured = json.loads(subprocess.check_output([worker, 'bench', path, '1'], stderr=subprocess.PIPE))
                    self.assertEqual(measured['digest'], scalar_trace_checksum(trace), (worker.name, path.name))
                    self.assertEqual(measured['packets'], len(trace['packets']))
                    self.assertEqual(measured['bytes'], sum(len(p['payload']) // 2 for p in trace['packets']))

    def test_reviewed_packet_views_checksum_all_their_selected_work(self):
        for case, reference in [('zlib-empty', 'ffmpeg-worker'), ('zlib-mixed', 'ffmpeg-combined-worker')]:
            path = ROOT / '.research/zlib-fixtures' / (case + '.mkv')
            for worker in [ROOT / 'target/release/media-worker', ROOT / 'target/harness' / reference]:
                trace = json.loads(subprocess.check_output([worker, 'codec-trace', path], stderr=subprocess.PIPE))
                measured = json.loads(subprocess.check_output([worker, 'codec-bench', path, '1'], stderr=subprocess.PIPE))
                self.assertEqual(measured['digest'], scalar_trace_checksum(trace))
                self.assertEqual(measured['packets'], len(trace['packets']))
                self.assertEqual(measured['bytes'], sum(len(p['payload']) // 2 for p in trace['packets']))
