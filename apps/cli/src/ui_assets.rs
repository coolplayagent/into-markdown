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
    sha256: "dc7ca4cb4abb41c6deea0497c7b0882a7a0ec3df7f73cdadd6a5f65cd770645a",
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
        path: "/assets/app.3c748e66242bc033.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "3c748e66242bc033a16f839cc3c33ad9502d19e4881a9cd635613689aa57cc5e",
        bytes: include_bytes!("../../../web/console/dist/assets/app.3c748e66242bc033.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.f049d1e543662aee.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "f049d1e543662aee96468a627cf2a76fdb9eed3f7065a3ebb45d53a7abf03c4f",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.f049d1e543662aee.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
