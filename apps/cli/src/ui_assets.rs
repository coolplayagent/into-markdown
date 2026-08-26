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
    sha256: "af8af9ec89066a2bd9c3ee71450e90f44ba9eee5fe7ba5495b1967c34611b664",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.62a4c012f2178828.css",
        mime: "text/css; charset=utf-8",
        sha256: "62a4c012f2178828b0dc9233ec8bfbb8f667f5175ae6129b1bff2e3f876c4d6f",
        bytes: include_bytes!("../../../web/console/dist/assets/app.62a4c012f2178828.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.edca651b6a03fb5f.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "edca651b6a03fb5fa261676bee7ae9f945b886df7cd3e5cedc9a4513b737d08e",
        bytes: include_bytes!("../../../web/console/dist/assets/app.edca651b6a03fb5f.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.80551442d7a6b747.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "80551442d7a6b747e630aa894723f310c166e5b2c78e9043454bfc926da29c4a",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.80551442d7a6b747.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
