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
    sha256: "97a702900722ce7e522137b0f9c40403b35e2331997b15fc44e0ce529db2ce79",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.621817ef81a1cb19.css",
        mime: "text/css; charset=utf-8",
        sha256: "621817ef81a1cb19c26b5156a55add2ab869904e6c9f0375baa0886baac7f3bb",
        bytes: include_bytes!("../../../web/console/dist/assets/app.621817ef81a1cb19.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.296fe231401075bf.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "296fe231401075bf13c35c1bc3e56cf6e2b9b4df04d571f2887ce2a79da3a4c8",
        bytes: include_bytes!("../../../web/console/dist/assets/app.296fe231401075bf.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.de16324a2604049b.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "de16324a2604049bea35a94494cb319052165363420023ce61653fbea92d5c7e",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.de16324a2604049b.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
