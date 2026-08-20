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
    sha256: "1ecaa33e7bab6e52b642f9e96b064b16bff742efc34947e30cfaf98a55f54060",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.4f1b1dfb8b56e285.css",
        mime: "text/css; charset=utf-8",
        sha256: "4f1b1dfb8b56e285976a6150c53b7e2061dfabe41163bd53a54c1a32e14ed48c",
        bytes: include_bytes!("../../../web/console/dist/assets/app.4f1b1dfb8b56e285.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.70f051ca91d78a7b.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "70f051ca91d78a7b4659f7fd806055ff29d2683f4dc9c29840dc234d07bd8ee2",
        bytes: include_bytes!("../../../web/console/dist/assets/app.70f051ca91d78a7b.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.7114f36de2c0da38.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "7114f36de2c0da389833bd1222e783c14a0963e94fb6960770fb572959f51c99",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.7114f36de2c0da38.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
