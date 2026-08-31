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
    sha256: "7d6ac2fc2043c5565d5fe9c63dd3add96c58236fd8d2fd7fae226986497b2a99",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.c0776ec608dc945c.css",
        mime: "text/css; charset=utf-8",
        sha256: "c0776ec608dc945c44bd7cbbf0e590d4b573fdae4c984571667ca64fb791d6b4",
        bytes: include_bytes!("../../../web/console/dist/assets/app.c0776ec608dc945c.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.60445e72b1d55b5f.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "60445e72b1d55b5f0bbf26e7d2509690ac3ad2c1105cd976e843b174bef89848",
        bytes: include_bytes!("../../../web/console/dist/assets/app.60445e72b1d55b5f.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.fe9923e9f1ef4948.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "fe9923e9f1ef4948450c21e4033c3ff93690a322eee60389021a5786efc9004f",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.fe9923e9f1ef4948.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
