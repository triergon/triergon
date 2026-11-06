//! Codec selection from a literal load payload.
//!
//! These are fixed matching rules, not a payload grammar: video searches the whole lowercased payload for
//! `h265`/`hevc`, everything else is H.264; audio reads the first literal
//! `"audio":"` value, case-sensitively, and unknown names select no audio.
use triergon_codec::{AudioCodec, VideoCodec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSelection {
    pub video: VideoCodec,
    pub audio: Option<AudioCodec>,
}

pub fn select(payload: &str) -> LoadSelection {
    let lower = payload.to_ascii_lowercase();
    let video = if lower.contains("h265") || lower.contains("hevc") {
        VideoCodec::Hevc
    } else {
        VideoCodec::H264
    };
    LoadSelection {
        video,
        audio: audio_name(payload).and_then(|name| match name {
            "AC3" => Some(AudioCodec::Ac3),
            "AC3 PLUS" => Some(AudioCodec::Eac3),
            "AAC" => Some(AudioCodec::Aac),
            "DTS" => Some(AudioCodec::Dts),
            _ => None,
        }),
    }
}

fn audio_name(payload: &str) -> Option<&str> {
    const KEY: &str = "\"audio\":\"";
    let at = payload.find(KEY)? + KEY.len();
    let rest = &payload[at..];
    Some(&rest[..rest.find('"')?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(video: &str, audio: &str) -> String {
        format!(r#"{{"codec":{{"video":"{video}","audio":"{audio}"}}}}"#)
    }

    #[test]
    fn every_audio_lane_the_payload_can_name_has_a_decoder() {
        for (name, codec) in [
            ("AC3", AudioCodec::Ac3),
            ("AC3 PLUS", AudioCodec::Eac3),
            ("AAC", AudioCodec::Aac),
            ("DTS", AudioCodec::Dts),
        ] {
            assert_eq!(select(&payload("H264", name)).audio, Some(codec));
        }
        assert_eq!(select(r#"{"codec":{"video":"H264"}}"#).audio, None);
        assert_eq!(
            select(&payload("H264", "aac")).audio,
            None,
            "case-sensitive like the source"
        );
        assert_eq!(select(&payload("H264", "OPUS")).audio, None);
    }

    #[test]
    fn video_follows_the_lowercased_whole_payload_search() {
        assert_eq!(select(&payload("HEVC", "AAC")).video, VideoCodec::Hevc);
        assert_eq!(select(&payload("H265", "AAC")).video, VideoCodec::Hevc);
        assert_eq!(
            select(r#"{"title":"my hevc rip","codec":{"video":"H264"}}"#).video,
            VideoCodec::Hevc
        );
        assert_eq!(select(&payload("H264", "AAC")).video, VideoCodec::H264);
        assert_eq!(
            select(&payload("VP9", "AAC")).video,
            VideoCodec::H264,
            "source fallback"
        );
    }
}
