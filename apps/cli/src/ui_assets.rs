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
    sha256: "24e0497d0a0f0e0d25bea260e1cee78d42b4701ad02c9b9d60b722a6adc03e2c",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.5f489cca7d184383.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "5f489cca7d184383060f1fd3d570a56b5f4c269a04a6af16e99c4fbd25d4bcef",
        bytes: include_bytes!("../../../web/console/dist/assets/app.5f489cca7d184383.js"),
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
        path: "/assets/bootstrap.a26df7d135a3694a.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "a26df7d135a3694ac37f71cc0b22435f346e2f83876f36f222fb6818dfe0ea5e",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.a26df7d135a3694a.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
