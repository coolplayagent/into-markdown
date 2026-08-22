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
    sha256: "e6230a89a7103a3a25c2499ea6ad570d2582a7e796dcaa95d6866d68a5e58a2b",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.39cddcad8ea7ca79.css",
        mime: "text/css; charset=utf-8",
        sha256: "39cddcad8ea7ca79aad32ee3cfb1c1b646bb257517bb227692f299ac4702fc2a",
        bytes: include_bytes!("../../../web/console/dist/assets/app.39cddcad8ea7ca79.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.fceddf7033d57949.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "fceddf7033d57949cc5c5dbf4ff8328c1384c7c5b8596368ab47c79ae66cae34",
        bytes: include_bytes!("../../../web/console/dist/assets/app.fceddf7033d57949.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.85e601e99e419cf2.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "85e601e99e419cf2b263894f3deaf5586729e2a5eec0f32df8e9032ab4266c00",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.85e601e99e419cf2.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
