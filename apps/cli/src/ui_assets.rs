//! Checked-in Web assets embedded into every CLI package by `include_bytes!`.

pub struct Asset {
    pub path: &'static str,
    pub mime: &'static str,
    pub sha256: &'static str,
    pub bytes: &'static [u8],
    pub immutable: bool,
}

pub const INDEX: Asset = Asset {
    path: "/index.html",
    mime: "text/html; charset=utf-8",
    sha256: "d7f822fff61ab4cb2180bb57370b2fbf66c840b48be8d70bbaa5d35dcd8952bc",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.a29bc7b78628b539.css",
        mime: "text/css; charset=utf-8",
        sha256: "a29bc7b78628b539e1f227d355612910736c79face8cbb3824656cfb13f8bd21",
        bytes: include_bytes!("../../../web/console/dist/assets/app.a29bc7b78628b539.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.58de5e0f008efe9f.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "58de5e0f008efe9f6d7ad980cab639852b1da876c6dd3743c7a81fee4c4ca233",
        bytes: include_bytes!("../../../web/console/dist/assets/app.58de5e0f008efe9f.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.02b9b58717de0be1.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "02b9b58717de0be1ca1b055b6a446164fcd4385a8b7d795717b75f6d7b025835",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.02b9b58717de0be1.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
