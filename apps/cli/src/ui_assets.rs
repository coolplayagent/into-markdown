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
    sha256: "651c7a14fc1e02cb28b00128980696f55a4f820456d147473558516a313fb72c",
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
        path: "/assets/app.ea8cefa6ae409012.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "ea8cefa6ae4090123644ed1e752db076f8f3ebc2969aec7928fc976862f48bd5",
        bytes: include_bytes!("../../../web/console/dist/assets/app.ea8cefa6ae409012.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.cca1c9ef6d512433.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "cca1c9ef6d51243399022f40815a56128e330228040a0fadd5759ee1af6789e7",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.cca1c9ef6d512433.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
