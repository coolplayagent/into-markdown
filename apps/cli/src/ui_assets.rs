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
    sha256: "8368d386207547fc02fa0440b4e92f516ecf3fe24d016cae72333788cad2d561",
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
        path: "/assets/app.13c39d076d44ec59.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "13c39d076d44ec596a51841b542032b84f73e808927c30c4fc3737d7aba1b8e7",
        bytes: include_bytes!("../../../web/console/dist/assets/app.13c39d076d44ec59.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.d3fb04cfb97caf03.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "d3fb04cfb97caf0359e504b439442bcc86bd4c3c795a22d0c344768df99e54a7",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.d3fb04cfb97caf03.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
