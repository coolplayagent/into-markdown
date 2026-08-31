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
    sha256: "c38c40777ff1653816d4795553b28a4cf372a7d899a99d749de9ab40415dd4ba",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.9b4d0a39248559e7.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "9b4d0a39248559e75dbcfd64ad3720c88217a5defaa747e91304fe7bcb37f88e",
        bytes: include_bytes!("../../../web/console/dist/assets/app.9b4d0a39248559e7.js"),
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
        path: "/assets/bootstrap.0a523f0d30675cca.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "0a523f0d30675ccac31cc153144b0a1b7a4f8cf25440566b1a56e8b48eb3b994",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.0a523f0d30675cca.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
