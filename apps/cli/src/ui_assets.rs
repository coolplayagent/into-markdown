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
    sha256: "b13849ed35cf6c4ba63d226ae6e29024398a3f16d6f8e3b8b5b90891b09fd3b4",
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
        path: "/assets/app.eabe5704040d0a16.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "eabe5704040d0a16a6fe51f7a2c3456003c35b3d2671256d938ab9e356dcdd58",
        bytes: include_bytes!("../../../web/console/dist/assets/app.eabe5704040d0a16.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.9d83686b25328d44.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "9d83686b25328d441b5f2d0028da65ffc3a2e162a21ea1ef812599c0f8b94ce6",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.9d83686b25328d44.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
