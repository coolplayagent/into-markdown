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
    sha256: "440e08ca3fda14c3e30ca8eff42817c100bb4d5f717f916593481469bc19aec2",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.4003de3d0795e994.css",
        mime: "text/css; charset=utf-8",
        sha256: "4003de3d0795e99429c230f73c53abe188a0ad0c877d69830aac4dba8f7bbeec",
        bytes: include_bytes!("../../../web/console/dist/assets/app.4003de3d0795e994.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.234afd0db4d204e9.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "234afd0db4d204e94fffe1423b7883bd7695bc2389d09e05bc2f5e04a054ef66",
        bytes: include_bytes!("../../../web/console/dist/assets/app.234afd0db4d204e9.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.594fd2b7af1e6b1a.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "594fd2b7af1e6b1ad37f1cb64079b27f3aa1c287af316c57f99ba4c4efe0ec1e",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.594fd2b7af1e6b1a.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
