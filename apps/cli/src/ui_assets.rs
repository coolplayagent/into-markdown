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
    sha256: "c0404206061e199021efcfa5d50415aa6cd1d158a823997417b26188cdb22b33",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.035d6c7e6c4c920c.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "035d6c7e6c4c920c1ae1ec5edb1e01c627ba6a830cf5e7e8f0ca59987f0b2abb",
        bytes: include_bytes!("../../../web/console/dist/assets/app.035d6c7e6c4c920c.js"),
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
        path: "/assets/bootstrap.525a7d2740672f1d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "525a7d2740672f1d4f178319911f966b392e838131cd25219fcedc30b44387e0",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.525a7d2740672f1d.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
