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
    sha256: "2dd1141a65f3c2dc7b058f3a149f98f392a45e36a95f63cbb80b1715f3a10400",
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
        path: "/assets/app.d61e58afc2f5b52d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "d61e58afc2f5b52d2aebd24e973155994e78073737ecaf9017eee3408d66b622",
        bytes: include_bytes!("../../../web/console/dist/assets/app.d61e58afc2f5b52d.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.96e8c18d80bb1c30.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "96e8c18d80bb1c307c757942add092cf4672730c5de7b4e0789852b974d28111",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.96e8c18d80bb1c30.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
