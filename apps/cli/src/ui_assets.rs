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
    sha256: "3a890141c96d699a646fed9a86209687bc7085103a6c0f7bf930274f9f1d68c5",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.85c8b12b59a5848b.css",
        mime: "text/css; charset=utf-8",
        sha256: "85c8b12b59a5848ba3d77b7b3bf27eebd402a803244da8560e3f7c6bbcb0886f",
        bytes: include_bytes!("../../../web/console/dist/assets/app.85c8b12b59a5848b.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.cefca4e701b8c157.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "cefca4e701b8c15738f1a4a14c559d49e0701010afa028878dcd2db4027d47ba",
        bytes: include_bytes!("../../../web/console/dist/assets/app.cefca4e701b8c157.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.1cb4571633778dfa.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "1cb4571633778dfa15034f2cecbf7d5f78ad62e23b5dbf678df5cda5b1f2a22f",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.1cb4571633778dfa.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
