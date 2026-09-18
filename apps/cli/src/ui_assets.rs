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
    sha256: "923daf83849280fbb36b18e629ef6f0af777fdfdb6f2ba45abee9a49efd60e32",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.310c1fca01bd0277.css",
        mime: "text/css; charset=utf-8",
        sha256: "310c1fca01bd0277f1a4df86ddb76796d0777c36b7e285e1c97b3e6a9057cfd0",
        bytes: include_bytes!("../../../web/console/dist/assets/app.310c1fca01bd0277.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.31d05cdcd1ac41ab.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "31d05cdcd1ac41ab029132df2c52824d05ab5d324a3ba9b51f3f89066333964e",
        bytes: include_bytes!("../../../web/console/dist/assets/app.31d05cdcd1ac41ab.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.8db86a41ac4b7347.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "8db86a41ac4b7347607d20b008039b62f283dd98c6f85816a4bf49ac60255a27",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.8db86a41ac4b7347.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
