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
    sha256: "1e5722fe9530cfd95d143452fa65174025c4f8a9b303b4c34f860719c4e52eb9",
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
        path: "/assets/app.850351639e494153.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "850351639e494153675dcf6beb24a2d113c1a2925c9a2f7be08aff0cebaea444",
        bytes: include_bytes!("../../../web/console/dist/assets/app.850351639e494153.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.16e2143de688caf1.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "16e2143de688caf1816d83ffa490ba081f41fd3c38ec1f5128286801b41380cf",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.16e2143de688caf1.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
