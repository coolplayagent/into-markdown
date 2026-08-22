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
    sha256: "ab28d203df93639c29732f31054b9754b35c9f6bb24d2945db8583a9f8612925",
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
        path: "/assets/app.18a896f0915c6a4d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "18a896f0915c6a4d25dc002ffa8a1b9fb1b80cec538c4f3113962c8b1516e277",
        bytes: include_bytes!("../../../web/console/dist/assets/app.18a896f0915c6a4d.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.827cf75ca606089d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "827cf75ca606089da51552ca5fb17a3852806f123a7b877167bfdab25e2b218f",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.827cf75ca606089d.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
