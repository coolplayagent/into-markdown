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
    sha256: "8f706a1066d53420e61638c09185dcf32d0bc0fa6077cd233b693f2b4fa0fdd8",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.62a4c012f2178828.css",
        mime: "text/css; charset=utf-8",
        sha256: "62a4c012f2178828b0dc9233ec8bfbb8f667f5175ae6129b1bff2e3f876c4d6f",
        bytes: include_bytes!("../../../web/console/dist/assets/app.62a4c012f2178828.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.3bff07ee77ff0de0.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "3bff07ee77ff0de0fcbb50559a17e62e1a9514d6a00db2816e140f1522bcfbe0",
        bytes: include_bytes!("../../../web/console/dist/assets/app.3bff07ee77ff0de0.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.6062f1100ce479be.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "6062f1100ce479be038b3b80b53876c3749254a93fd0dec1afb8c64181dc49b6",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.6062f1100ce479be.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
