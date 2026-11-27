//! Authored ownership, storage limit and work limit fixtures.

use std::path::Path;

use super::bits::Cases;

pub(super) fn fixtures(root: &Path, write: bool) -> Result<(), String> {
    let mut cases = Cases::new(root, "core-budgets")?;
    for (id, profile, input) in [
        ("no-control-space", "storage", "bare 0\n"),
        (
            "empty-pool",
            "storage",
            "storage 0\nreserve a 0\nreserve b 1\nrelease a\n",
        ),
        (
            "until-release",
            "storage",
            "storage 32\nreserve a 20\nreserve b 20\nreserve c 33\nreserve b 12\nresize a 21\nresize a 8\nreserve c 12\nrelease a\nresize b 20\nrelease b\nrelease c\nreserve d 32\nrelease d\n",
        ),
        (
            "room-to-spare",
            "storage",
            "storage 4096\nreserve a 256\nreserve b 127\nresize a 2048\nrelease a\nresize b 0\nrelease b\n",
        ),
        (
            "shared-storage",
            "owner",
            "owner 32\nbytes a 32\nclone b a\nsplit c b 3 8\nsplit bad c 2 6\nreserve d 1\nrelease a\nrelease b\nreserve d 1\nrelease c\nbytes e 0\nrelease e\n",
        ),
        (
            "freeze-retains-storage",
            "owner",
            "owner 128\nvector a\ngrow a 12\nfill a 12\nfreeze a\nclone b a\nsplit c b 3 3\nrelease a\nrelease b\nrelease c\n",
        ),
        (
            "vector-replacement",
            "storage",
            "storage 50\nvector a\ngrow a 20\nfill a 10\ngrow a 21\ngrow a 20\nclone b a\nfill a 15\nrelease b\nfill a 15\nrelease a\n",
        ),
        (
            "zero-work",
            "work",
            "work 0\ntry_charge 0\ntry_charge 1\ncharge 1\ncharge 0\n",
        ),
        (
            "work-exhaustion",
            "work",
            "work 64\ntry_charge 65\ncharge 17\ntry_charge 48\ntry_charge 47\ncharge 1\ntry_charge 0\n",
        ),
        (
            "full-width-work",
            "work",
            "work 18446744073709551615\ncharge 18446744073709551614\ntry_charge 2\ncharge 2\ntry_charge 1\n",
        ),
    ] {
        cases.add(id, profile, input.as_bytes(), &[])?;
    }
    cases.manifest(write)
}
