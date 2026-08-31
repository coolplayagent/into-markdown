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
    sha256: "856e29417cd948fead0746ed666672bb05da507271925085d6c2b11b151cf3b0",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.c673f383a2c089ca.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "c673f383a2c089cad01ec85cb2995503f01594ec342e9307a4472898f2225721",
        bytes: include_bytes!("../../../web/console/dist/assets/app.c673f383a2c089ca.js"),
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
        path: "/assets/bootstrap.afc06a3700f4b7a1.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "afc06a3700f4b7a10124fa70e528fceb4ec9b07ebf63099384fe54dd1ef3b9fd",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.afc06a3700f4b7a1.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
