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
    sha256: "4f83195aefd3383c6d0571904956ba58ff83cfff635207b46a0ced141cc49dc3",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.4f07a6d9dd04fdae.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "4f07a6d9dd04fdaef6ad76bb91792ac6acf97524d3261374c3007c675e36466c",
        bytes: include_bytes!("../../../web/console/dist/assets/app.4f07a6d9dd04fdae.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.c0776ec608dc945c.css",
        mime: "text/css; charset=utf-8",
        sha256: "c0776ec608dc945c44bd7cbbf0e590d4b573fdae4c984571667ca64fb791d6b4",
        bytes: include_bytes!("../../../web/console/dist/assets/app.c0776ec608dc945c.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.156526d616a18178.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "156526d616a18178789ed5fa0abff822e9d46805d9bfc6010e92416d7b8c31df",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.156526d616a18178.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
