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
    sha256: "7ab30db150a15bd10a210cc830f39d990abbb6f8b731c7a1165325ec0fb55f70",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.8706d562421fefdb.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "8706d562421fefdb077758dd7bfd10e9f587aa86b5f85ad15abd9d22c3a9c998",
        bytes: include_bytes!("../../../web/console/dist/assets/app.8706d562421fefdb.js"),
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
        path: "/assets/bootstrap.1e55db1631ebd9fb.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "1e55db1631ebd9fb0347904b6d4d26c7460f10b9a2e50959dc92367205d737fe",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.1e55db1631ebd9fb.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
