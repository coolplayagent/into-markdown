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
    sha256: "34ecce803d6aa0634994fbc1efdbb5746d5cc4adbef77ffca2ea84c7abdc8d46",
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
        path: "/assets/app.faa500d242efdca5.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "faa500d242efdca507b80304124071c55199d09596fe322162c11e0e9c36d3f6",
        bytes: include_bytes!("../../../web/console/dist/assets/app.faa500d242efdca5.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.24dc08574199a746.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "24dc08574199a7461bdd57354e5a3d53eab239f44a8d90e6454883f0ec48c729",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.24dc08574199a746.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
