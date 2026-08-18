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
    sha256: "a150c9a2918380f28605f152516a7de5238d6add63e0569d2aa0a31aab40a339",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.470d4e654bc105de.css",
        mime: "text/css; charset=utf-8",
        sha256: "470d4e654bc105de228b48a94be7cff0ee1a9b90a2219aee4b98e1c3a3f5b98a",
        bytes: include_bytes!("../../../web/console/dist/assets/app.470d4e654bc105de.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.e216df6f919396ca.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "e216df6f919396ca3a3451dca2114bf7dec139575b1bd4905498d19e361a5c92",
        bytes: include_bytes!("../../../web/console/dist/assets/app.e216df6f919396ca.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.04afa957dfc359be.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "04afa957dfc359be35c797f12a43a70141212df4a472b1d0578cefd147c8c18a",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.04afa957dfc359be.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
