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
    sha256: "675d7a3474f821790d34ccce31fed90417c0bedf25b4fa01bc02b2c7394e275f",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.1c194f3850fe9256.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "1c194f3850fe92565d36d578becf7635cc2a79b41c0e18db6113627cfa635db3",
        bytes: include_bytes!("../../../web/console/dist/assets/app.1c194f3850fe9256.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.4a06d2c9fb9e4d6c.css",
        mime: "text/css; charset=utf-8",
        sha256: "4a06d2c9fb9e4d6ce7c91547447fd629b1d26bafd3fc6c4dcabb1e2de76a3c3b",
        bytes: include_bytes!("../../../web/console/dist/assets/app.4a06d2c9fb9e4d6c.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.ea47e824b9656af3.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "ea47e824b9656af3defada8a930d9cf7d0fa1e79a9e32670c812e84f47ec5182",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.ea47e824b9656af3.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
