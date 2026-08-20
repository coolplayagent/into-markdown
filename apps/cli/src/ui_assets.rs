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
    sha256: "396c59fd0f3ed528abda8e75c4b9ca14eba7c0a19c965c22dcd0a9630d4ea50a",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.36d643c24a9b96b2.css",
        mime: "text/css; charset=utf-8",
        sha256: "36d643c24a9b96b2cf0c0d353030ddee66ecd47d1383acd58e07701bfbe8b72f",
        bytes: include_bytes!("../../../web/console/dist/assets/app.36d643c24a9b96b2.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.e04f62eef75fefbe.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "e04f62eef75fefbebd0e6debf40ccb7d11a91e942745b21ed6d8313d6c29e404",
        bytes: include_bytes!("../../../web/console/dist/assets/app.e04f62eef75fefbe.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.a266028721d23a91.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "a266028721d23a91b4d7995ff4e366826700139a649cb85d7fc017c5590dc33e",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.a266028721d23a91.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
