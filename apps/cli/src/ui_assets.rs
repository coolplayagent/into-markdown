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
    sha256: "1a7038f7049eef2f7176e1bc82692fd1baf947fcdad9c73c94ed02fc374bf4e3",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.310c1fca01bd0277.css",
        mime: "text/css; charset=utf-8",
        sha256: "310c1fca01bd0277f1a4df86ddb76796d0777c36b7e285e1c97b3e6a9057cfd0",
        bytes: include_bytes!("../../../web/console/dist/assets/app.310c1fca01bd0277.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.c36f4f18d7278abf.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "c36f4f18d7278abfd9ee6b585a1fbd259c949e70ed6cb1b72110727854aba904",
        bytes: include_bytes!("../../../web/console/dist/assets/app.c36f4f18d7278abf.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.587700a0f380e91d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "587700a0f380e91d29ad5948763b3e7d525cc0c3353ae252a8b9ad91e13d1e11",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.587700a0f380e91d.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
