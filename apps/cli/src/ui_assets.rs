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
    sha256: "ac06323dd79566059b49dea07bd596d6bc32b87a4fe260958567fee7521de095",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.73bec4e67823da88.css",
        mime: "text/css; charset=utf-8",
        sha256: "73bec4e67823da881b183ef40b1402d897ba3f0c660eac3f6e7b1e8537409d6f",
        bytes: include_bytes!("../../../web/console/dist/assets/app.73bec4e67823da88.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.8dad1c1a48a1e296.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "8dad1c1a48a1e296fb9d9a33a3e033ecbd66f0b25fb6b0d5d9438060ebe5ae9d",
        bytes: include_bytes!("../../../web/console/dist/assets/app.8dad1c1a48a1e296.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.926f21e9251d4aa0.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "926f21e9251d4aa038c36fa935a513ee77c7d2ad08a68ca06fb3cf62147ab762",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.926f21e9251d4aa0.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
