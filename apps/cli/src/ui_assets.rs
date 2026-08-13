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
    sha256: "4e767d02eb8146f8a8abbeed03215385b607f61fdaa73f28af45e2d8ba766020",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.0f97f210b0bda92b.css",
        mime: "text/css; charset=utf-8",
        sha256: "0f97f210b0bda92b80e08591dc5fa42af75299091fc4a0abedfc401b3dfbe7de",
        bytes: include_bytes!("../../../web/console/dist/assets/app.0f97f210b0bda92b.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.c5453d98f3e3ee06.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "c5453d98f3e3ee0609d449e6644d6eb3c9d83a32780178321492580d696aaf14",
        bytes: include_bytes!("../../../web/console/dist/assets/app.c5453d98f3e3ee06.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.2d8c0e73bc170a2a.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "2d8c0e73bc170a2ab03e6b36e4429c8639f6146462ad74da98cae3189e4961ea",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.2d8c0e73bc170a2a.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
