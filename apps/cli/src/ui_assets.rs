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
    sha256: "759799af873765e8012e46dac0f40211a6dff9d41686a2983a97942841fdbc33",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.ff87db1fce4c67c3.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "ff87db1fce4c67c3656eb765273785048960a4a394b9edbafb53e9b9f5117cdd",
        bytes: include_bytes!("../../../web/console/dist/assets/app.ff87db1fce4c67c3.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.c0776ec608dc945c.css",
        mime: "text/css; charset=utf-8",
        sha256: "c0776ec608dc945c44bd7cbbf0e590d4b573fdae4c984571667ca64fb791d6b4",
        bytes: include_bytes!("../../../web/console/dist/assets/app.c0776ec608dc945c.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.cdb92afa209cd28e.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "cdb92afa209cd28e7d4b9d242a00443855db84f1634a8373fcca56baa5c7ddf1",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.cdb92afa209cd28e.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
