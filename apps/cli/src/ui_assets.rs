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
    sha256: "f18fd6564f07fca7083c2e12a0d978043504e53e81d7c0c6adb7b8877ee7bc98",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.f205ee673998c673.css",
        mime: "text/css; charset=utf-8",
        sha256: "f205ee673998c6732c2089d97190ca3ea1e68fd8225d35524231799f8da5889d",
        bytes: include_bytes!("../../../web/console/dist/assets/app.f205ee673998c673.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.1f993805c976df09.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "1f993805c976df096653d0c289a09a6dffc84a32b52bab0dbfd687dccaa24669",
        bytes: include_bytes!("../../../web/console/dist/assets/app.1f993805c976df09.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.63383b893163f97a.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "63383b893163f97ac3508a6cf521d58c6cd6f9b778616a8aa6c6d5bb9e59c843",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.63383b893163f97a.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
