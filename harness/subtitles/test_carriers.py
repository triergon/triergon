"""Wire-format boundaries used by the subtitle fixture writers."""

import unittest

from harness.subtitles import dvb_streams, dvd_streams, pgs_streams, teletext_streams


class CarrierTests(unittest.TestCase):
    def test_mpeg_crc(self):
        for writer in (pgs_streams, dvb_streams, teletext_streams):
            with self.subTest(writer=writer.__name__):
                self.assertEqual(writer.crc32_mpeg(b""), 0xFFFFFFFF)
                self.assertEqual(writer.crc32_mpeg(iter(b"123456789")), 0x0376E6E7)
                self.assertEqual(writer.crc32_mpeg(b"123456789\x03\x76\xe6\xe7"), 0)

    def test_ebml_ids(self):
        for writer in (pgs_streams, dvd_streams):
            with self.subTest(writer=writer.__name__):
                self.assertEqual(writer.ebml_id(0), b"")
                self.assertEqual(writer.ebml_id(0x1A45DFA3), b"\x1a\x45\xdf\xa3")
                self.assertEqual(writer.ebml_id(1 << 64), b"\x01" + bytes(8))
                with self.assertRaises(OverflowError):
                    writer.ebml_id(-1)

    def test_ebml_sizes_skip_reserved_values(self):
        for writer in (pgs_streams, dvd_streams):
            with self.subTest(writer=writer.__name__):
                self.assertEqual(writer.ebml_size(0), b"\x80")
                with self.assertRaises(OverflowError):
                    writer.ebml_size(-1)
                for width in range(1, 9):
                    reserved = (1 << (7 * width)) - 1
                    self.assertEqual(writer.ebml_size(reserved - 1),
                                     (reserved << 1).to_bytes(width, "big"))
                    if width < 8:
                        marker = 1 << (7 * (width + 1))
                        self.assertEqual(writer.ebml_size(reserved), (marker | reserved).to_bytes(width + 1, "big"))
                for size in [(1 << 56) - 1, 1 << 56]:
                    with self.assertRaises(ValueError) as error:
                        writer.ebml_size(size)
                    self.assertEqual(error.exception.args, (size,))
