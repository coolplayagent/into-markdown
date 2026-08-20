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
    sha256: "fe940558414b5cd815d617b05ac481a807f494b707c707e76bb462ee0792f06f",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.07c1e1139680b7ae.css",
        mime: "text/css; charset=utf-8",
        sha256: "07c1e1139680b7ae65a01dd5e6ea178cf13ac8996812bfe0e571c3dfd7ee8892",
        bytes: include_bytes!("../../../web/console/dist/assets/app.07c1e1139680b7ae.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.3be8aba669e96e4c.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "3be8aba669e96e4c105ae948824af4369d30daad5f65d229e145fc0cb97a9cdc",
        bytes: include_bytes!("../../../web/console/dist/assets/app.3be8aba669e96e4c.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.5837d7d04e1c266c.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "5837d7d04e1c266c98da9aa7155e1e5fcc4a8cf1eccea0b12dc1311ed91949f6",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.5837d7d04e1c266c.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
