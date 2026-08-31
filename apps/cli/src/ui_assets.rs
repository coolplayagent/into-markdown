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
    sha256: "24fea16f372fcd70e0af6c5b6ce2450054a9672e4db7cd4d67eeb92cc49be5bf",
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
        path: "/assets/app.bd21406250ed8891.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "bd21406250ed889114e935db6b6e6585fdd16ceb96c4587644dcc1302f0058e8",
        bytes: include_bytes!("../../../web/console/dist/assets/app.bd21406250ed8891.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.3915864f99ea7f1a.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "3915864f99ea7f1af83d18b1d17d132f7887e04677607cfd1e6e8715b8bd14e2",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.3915864f99ea7f1a.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
