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
    sha256: "028b01cfdf82ce71dfac73d06b7c0e8a356aaa358f7121ac0429a09e65ab7911",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.4b48a7257480403b.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "4b48a7257480403bd4c4b3acb79dfec0592eeae916c3d12fb8ccb0ac4e4bf78e",
        bytes: include_bytes!("../../../web/console/dist/assets/app.4b48a7257480403b.js"),
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
        path: "/assets/bootstrap.0661957a8d686a6b.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "0661957a8d686a6ba47d491bfb50aad301dbc9df8dc1bcb555cf6e569ce43b9a",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.0661957a8d686a6b.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
