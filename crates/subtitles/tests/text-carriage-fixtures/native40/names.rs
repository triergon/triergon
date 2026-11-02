#[test]
fn native_utf16_big_ascii() { run_case(0); }
#[test]
fn native_utf16_little_ascii() { run_case(1); }
#[test]
fn native_utf16_big_bmp_first_00c5() { run_case(2); }
#[test]
fn native_utf16_little_bmp_first_00c5() { run_case(3); }
#[test]
fn native_utf16_big_bmp_first_00e9() { run_case(4); }
#[test]
fn native_utf16_little_bmp_first_00e9() { run_case(5); }
#[test]
fn native_utf16_big_bmp_middle_00c5() { run_case(6); }
#[test]
fn native_utf16_little_bmp_middle_00c5() { run_case(7); }
#[test]
fn native_utf16_big_bmp_first_0145() { run_case(8); }
#[test]
fn native_utf16_little_bmp_first_0145() { run_case(9); }
#[test]
fn native_utf16_big_bmp_first_6c34() { run_case(10); }
#[test]
fn native_utf16_little_bmp_first_6c34() { run_case(11); }
#[test]
fn native_utf16_big_supplementary_first() { run_case(12); }
#[test]
fn native_utf16_little_supplementary_first() { run_case(13); }
#[test]
fn native_utf16_big_multiline_00c5() { run_case(14); }
#[test]
fn native_utf16_little_multiline_00c5() { run_case(15); }
#[test]
fn native_utf8_ascii() { run_case(16); }
#[test]
fn native_utf8_bom_ascii() { run_case(17); }
#[test]
fn native_utf8_bmp_first_00c5() { run_case(18); }
#[test]
fn native_utf8_bom_bmp_first_00c5() { run_case(19); }
#[test]
fn native_utf8_bmp_first_00e9() { run_case(20); }
#[test]
fn native_utf8_bom_bmp_first_00e9() { run_case(21); }
#[test]
fn native_utf8_bmp_middle_00c5() { run_case(22); }
#[test]
fn native_utf8_bom_bmp_middle_00c5() { run_case(23); }
#[test]
fn native_utf8_bmp_first_0145() { run_case(24); }
#[test]
fn native_utf8_bom_bmp_first_0145() { run_case(25); }
#[test]
fn native_utf8_bmp_first_6c34() { run_case(26); }
#[test]
fn native_utf8_bom_bmp_first_6c34() { run_case(27); }
#[test]
fn native_utf8_supplementary_first() { run_case(28); }
#[test]
fn native_utf8_bom_supplementary_first() { run_case(29); }
#[test]
fn native_utf8_multiline_00c5() { run_case(30); }
#[test]
fn native_utf8_bom_multiline_00c5() { run_case(31); }
#[test]
fn native_utf16_big_scalar_0085_middle() { run_case(32); }
#[test]
fn native_utf16_little_scalar_0085_middle() { run_case(33); }
#[test]
fn native_utf8_scalar_0085_middle() { run_case(34); }
#[test]
fn native_utf8_bom_scalar_0085_middle() { run_case(35); }
#[test]
fn native_packet_00c5() { run_case(36); }
#[test]
fn native_packet_0145() { run_case(37); }
#[test]
fn native_packet_6c34() { run_case(38); }
#[test]
fn native_packet_1f600() { run_case(39); }
