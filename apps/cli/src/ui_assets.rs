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
    sha256: "cba8eaf48c13f6eefddf9193fd774fba2eb1592090108dd964f8e550b9d2f7ab",
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
        path: "/assets/app.9dbefaee81b198d5.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "9dbefaee81b198d507758b8879e30d470693ab84a961c3188ffc72823fe19419",
        bytes: include_bytes!("../../../web/console/dist/assets/app.9dbefaee81b198d5.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.de837d95b45d02e9.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "de837d95b45d02e914933bab96b10db3b4424916facc26209c3d4614e02af628",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.de837d95b45d02e9.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
