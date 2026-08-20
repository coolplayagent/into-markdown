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
    sha256: "865fd5c8eca49b0fa33a97c5ed9d6e6650ff3529d10c1627e0b49cc9132c21cd",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.04156748a7a3f318.css",
        mime: "text/css; charset=utf-8",
        sha256: "04156748a7a3f3187ca221ab2eaf2fb46d04c415f0766adb82e8c14a8397c477",
        bytes: include_bytes!("../../../web/console/dist/assets/app.04156748a7a3f318.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.7d2071377f993866.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "7d2071377f993866d39affbbff60368737188750e4d212ac5e8dc70baf6032aa",
        bytes: include_bytes!("../../../web/console/dist/assets/app.7d2071377f993866.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.bd54284ee4526e07.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "bd54284ee4526e07dc0459c99a248a655c8f36c32d191f4dea9d553c390ebb5e",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.bd54284ee4526e07.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
