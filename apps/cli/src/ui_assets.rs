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
    sha256: "658f282f1c4f33cd792c970dd6e01bbbf3580875e462741b1ba8a21fce33ca9f",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.470d4e654bc105de.css",
        mime: "text/css; charset=utf-8",
        sha256: "470d4e654bc105de228b48a94be7cff0ee1a9b90a2219aee4b98e1c3a3f5b98a",
        bytes: include_bytes!("../../../web/console/dist/assets/app.470d4e654bc105de.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.954c0636c4dcb904.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "954c0636c4dcb9041449796f8371ecd1f619fe793804f8332350d8ba5680fdce",
        bytes: include_bytes!("../../../web/console/dist/assets/app.954c0636c4dcb904.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.417e45047dd58d7c.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "417e45047dd58d7c5d65e632d8e7c82a6a727abbad6a930b56c62d944573d34a",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.417e45047dd58d7c.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
