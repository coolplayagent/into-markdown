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
    sha256: "18c173df23939fefb139e36898b9040b6a8a9c8a0e016f280c2414a2bea3d808",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.ea7b34ab5ca6c3aa.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "ea7b34ab5ca6c3aaf23f651fca9b601e4cd30de9bbae17a0cacdcb1369731e05",
        bytes: include_bytes!("../../../web/console/dist/assets/app.ea7b34ab5ca6c3aa.js"),
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
        path: "/assets/bootstrap.1b3ad6a7de62a3cc.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "1b3ad6a7de62a3cc86bd65d6ee933898c28b87f5a7fe5baeccf94a9d8e6207c2",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.1b3ad6a7de62a3cc.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
