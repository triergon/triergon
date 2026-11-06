//! AC-3 program channel bound and layout names.
use triergon_codec::{ChannelLayout, Speaker};

/// Maximum number of output channels (A/52 Annex E 3.8.2: at most 16 per program).
pub const MAX_CHANNELS: usize = 16;

/// Conventional name of a well-known layout (e.g. `5.1(side)`, as ffprobe prints it),
/// else the speaker list.
pub fn layout_name(layout: &ChannelLayout) -> String {
    use Speaker::*;
    let known: &[(&[Speaker], &str)] = &[
        (&[FrontCenter], "mono"),
        (&[FrontLeft, FrontRight], "stereo"),
        (&[FrontLeft, FrontRight, LowFrequency], "2.1"),
        (&[FrontLeft, FrontRight, FrontCenter], "3.0"),
        (&[FrontLeft, FrontRight, BackCenter], "3.0(back)"),
        (&[FrontLeft, FrontRight, FrontCenter, BackCenter], "4.0"),
        (&[FrontLeft, FrontRight, SideLeft, SideRight], "quad(side)"),
        (&[FrontLeft, FrontRight, FrontCenter, LowFrequency], "3.1"),
        (
            &[FrontLeft, FrontRight, LowFrequency, BackCenter],
            "3.1(back)?",
        ),
        (
            &[FrontLeft, FrontRight, FrontCenter, LowFrequency, BackCenter],
            "4.1",
        ),
        (
            &[FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight],
            "5.0(side)",
        ),
        (
            &[FrontLeft, FrontRight, LowFrequency, SideLeft, SideRight],
            "4.1(side)?",
        ),
        (
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                SideLeft,
                SideRight,
            ],
            "5.1(side)",
        ),
        (
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                SideLeft,
                SideRight,
            ],
            "7.1",
        ),
    ];
    for (speakers, name) in known {
        if layout.speakers() == *speakers {
            return (*name).to_string();
        }
    }
    layout
        .speakers()
        .iter()
        .map(|s| s.name())
        .collect::<Vec<_>>()
        .join("+")
}
