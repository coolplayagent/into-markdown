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
    sha256: "d10cea5dedf43651e81cf9773a0f89e4f775fecd091ea34dee8d017869f75cdd",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.8a516acb49f80d12.css",
        mime: "text/css; charset=utf-8",
        sha256: "8a516acb49f80d12d477cb9ffe0e0fa4d20fac1e1d7f7c6607e77e1f2d531938",
        bytes: include_bytes!("../../../web/console/dist/assets/app.8a516acb49f80d12.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.780c8fb016b01b3d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "780c8fb016b01b3dad572b0970dcd79f2fd7216540f5326ff684773cbb7f0ad3",
        bytes: include_bytes!("../../../web/console/dist/assets/app.780c8fb016b01b3d.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.921013a741e7748d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "921013a741e7748df56aa5c33f99e37331659e25a06b8922826d9078e6e80482",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.921013a741e7748d.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
