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
    sha256: "5f3ff6710cb408b6eb41757ee08b9b282b755575da33e6c5722abaec5afc9512",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.2946dda9d38fe894.css",
        mime: "text/css; charset=utf-8",
        sha256: "2946dda9d38fe8943103511071cc6339762e0ecbf32459de98610d2c75dcaab9",
        bytes: include_bytes!("../../../web/console/dist/assets/app.2946dda9d38fe894.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.87c6bf0bb568f05a.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "87c6bf0bb568f05ac3fc7f65e05148db237942f000d4450a76e14457dfe82ea2",
        bytes: include_bytes!("../../../web/console/dist/assets/app.87c6bf0bb568f05a.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.5f2302dda53a7992.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "5f2302dda53a7992e5390e39085aa96afcd135fb7f5a003b9da55a9bbeebce71",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.5f2302dda53a7992.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
