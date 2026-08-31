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
    sha256: "6a2b02c9c805a2fe3a96ea8bacf8d8dbea0e1575395014b89b9b933a9168116b",
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
        path: "/assets/app.593fc71e7df2b74d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "593fc71e7df2b74d2f79b04d9bd3a8c9a211a2128d2fa5650989f49dec92c853",
        bytes: include_bytes!("../../../web/console/dist/assets/app.593fc71e7df2b74d.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.c55de7897abf2359.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "c55de7897abf2359f19dde341259e85916dc29935fc149bc380d86e0e66a0202",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.c55de7897abf2359.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
