//! Independently authored source facts. These files are opaque carriage inputs;
//! the font probe is deliberately not a conforming font or a rendering oracle.
pub const ASS: &[u8] = include_bytes!("input/authored-ass-header.bin");
pub const FONT: &[u8] = include_bytes!("input/opaque-font-probe.bin");
pub const UNKNOWN: &[u8] = include_bytes!("input/opaque-unknown-attachment.bin");
pub const FONT_NAME: &[u8] = include_bytes!("input/font-name.utf8");
pub const TRACK_NAME: &[u8] = include_bytes!("input/track-name.utf8");
pub const DESCRIPTION: &[u8] = include_bytes!("input/description.utf8");
pub const PACKET: &[u8] = include_bytes!("input/packet.bin");
