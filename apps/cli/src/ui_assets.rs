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
    sha256: "1f241010b64483372d989ebbdefd52564d1d94c560d7aa5d5960d4b500f8d9c2",
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
        path: "/assets/app.308b0d3b1186c021.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "308b0d3b1186c02178d7a6c4694cf450888c22ade697de849aa6eb46b0568f01",
        bytes: include_bytes!("../../../web/console/dist/assets/app.308b0d3b1186c021.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.7b3fda00fdd78d05.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "7b3fda00fdd78d053bbd4133eff545291a5f804c8a4cbf2f820bc0cc53438acd",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.7b3fda00fdd78d05.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
