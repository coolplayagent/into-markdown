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
    sha256: "deb23e0f01360c147a6eba8cb1916925730b525df60da8268dbe37a9523e4419",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.c0776ec608dc945c.css",
        mime: "text/css; charset=utf-8",
        sha256: "c0776ec608dc945c44bd7cbbf0e590d4b573fdae4c984571667ca64fb791d6b4",
        bytes: include_bytes!("../../../web/console/dist/assets/app.c0776ec608dc945c.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.fc458dd50c27abda.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "fc458dd50c27abdab31ffc910045022a52c1f3ce347968758504def93b5e4b2e",
        bytes: include_bytes!("../../../web/console/dist/assets/app.fc458dd50c27abda.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.4a8619b57fae53ff.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "4a8619b57fae53ff611188f3aee2ed46ac5054c7e6f1a74a4c75437afc6a384f",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.4a8619b57fae53ff.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
