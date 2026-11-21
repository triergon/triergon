use crate::ebml::{element as e, uint as u};

use super::side_data::{Cases, file, simple, track};

fn flag(id: u32, value: Option<u64>) -> Vec<u8> {
    value.map_or_else(Vec::new, |value| u(id, value))
}

fn chapter(hidden: Option<u64>) -> Vec<u8> {
    let second = e(
        0x80,
        &[
            e(0x85, b"Second title"),
            e(0x437c, b"eng"),
            e(0x437c, b"fra"),
        ]
        .concat(),
    );
    let translated = e(
        0x80,
        &[
            e(0x85, "Kapitel".as_bytes()),
            e(0x437c, b"ger"),
            e(0x437c, b"eng"),
            e(0x437d, b"de-DE"),
            e(0x437d, b"en-GB"),
        ]
        .concat(),
    );
    let nested = e(
        0xb6,
        &[
            u(0x73c4, 105),
            u(0x91, 3000),
            u(0x92, 8000),
            e(0x80, &e(0x85, b"Nested default language")),
        ]
        .concat(),
    );
    e(
        0xb6,
        &[
            u(0x73c4, 102),
            u(0x91, 2000),
            u(0x92, 9000),
            flag(0x98, hidden),
            translated,
            second,
            nested,
        ]
        .concat(),
    )
}

fn tag(target: Option<(u32, u64)>, name: &[u8], value: Option<u64>, ietf: bool) -> Vec<u8> {
    let targets = e(
        0x63c0,
        &[
            u(0x68ca, 30),
            e(0x63ca, b"TRACK"),
            target.map_or_else(Vec::new, |(id, uid)| u(id, uid)),
        ]
        .concat(),
    );
    let simple = e(
        0x67c8,
        &[
            e(0x45a3, name),
            e(0x4487, b"text-value"),
            e(0x447a, b"ger"),
            if ietf {
                e(0x447b, b"de-DE")
            } else {
                Vec::new()
            },
            flag(0x4484, value),
        ]
        .concat(),
    );
    e(0x7373, &[targets, simple].concat())
}

pub(super) fn cases() -> Cases {
    let mut cases = Cases::new();
    for (name, value) in [
        ("flags-absent", None),
        ("flags-zero", Some(0)),
        ("flags-one", Some(1)),
        ("languages-ietf", Some(1)),
    ] {
        let chapters = e(
            0x1043a770,
            &e(
                0x45b9,
                &[
                    u(0x45bc, 103),
                    flag(0x45db, value),
                    flag(0x45dd, value),
                    chapter(value),
                ]
                .concat(),
            ),
        );
        let attachment = |uid, file: &[u8], description: Option<&[u8]>| {
            e(
                0x61a7,
                &[
                    e(0x466e, file),
                    e(0x4660, b"application/octet-stream"),
                    u(0x46ae, uid),
                    e(0x465c, b"attachment payload"),
                    description.map_or_else(Vec::new, |desc| e(0x467e, desc)),
                ]
                .concat(),
            )
        };
        let attachments = e(
            0x1941a469,
            &[
                attachment(104, b"with.bin", Some(b"Description")),
                attachment(106, b"without.bin", None),
            ]
            .concat(),
        );
        let mut tags = [
            (None, b"GLOBAL".as_slice()),
            (Some((0x63c5, 101)), b"TRACK"),
            (Some((0x63c9, 103)), b"EDITION"),
            (Some((0x63c4, 102)), b"CHAPTER"),
            (Some((0x63c6, 104)), b"ATTACHMENT"),
        ]
        .into_iter()
        .flat_map(|(target, key)| tag(target, key, value, name == "languages-ietf"))
        .collect::<Vec<_>>();
        tags.extend(e(
            0x7373,
            &e(
                0x67c8,
                &[e(0x45a3, b"BINARY"), e(0x4485, &[0, 255, 0])].concat(),
            ),
        ));
        cases.push((
            name.to_owned(),
            file(
                false,
                &[track(1, b"V_VP9", &[])],
                &[chapters, attachments, e(0x1254c367, &tags)].concat(),
                &[simple(1, 0)],
            ),
        ));
    }
    cases
}
