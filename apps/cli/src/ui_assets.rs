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
    sha256: "b0172b8b76a4ed5df73e5813fe959cf1beac1ebeb561007e83af751e76e31f62",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.2c5bc3fa549e457d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "2c5bc3fa549e457dcd298eaa10d530d3a0fe3f109c4dbee49cffc981eccbe997",
        bytes: include_bytes!("../../../web/console/dist/assets/app.2c5bc3fa549e457d.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.4f1963a826167be3.css",
        mime: "text/css; charset=utf-8",
        sha256: "4f1963a826167be33603b57e6322da145421bb1a9477e00bf22618d42df2e156",
        bytes: include_bytes!("../../../web/console/dist/assets/app.4f1963a826167be3.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.6c29d8f83435c796.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "6c29d8f83435c7969343b43badf3c81d0335833cf4bf27ba0412477b46f5b5e3",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.6c29d8f83435c796.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
