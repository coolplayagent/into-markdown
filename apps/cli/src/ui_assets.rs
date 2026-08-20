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
    sha256: "fb1722238018ac7628bad02aa095f94142c4bb97d46c011d74da6356feb8e395",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.470d4e654bc105de.css",
        mime: "text/css; charset=utf-8",
        sha256: "470d4e654bc105de228b48a94be7cff0ee1a9b90a2219aee4b98e1c3a3f5b98a",
        bytes: include_bytes!("../../../web/console/dist/assets/app.470d4e654bc105de.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.ef5359d41bd46181.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "ef5359d41bd46181715ea2a44dd110833f2002f30e62963a5e1bbe8d1f1db235",
        bytes: include_bytes!("../../../web/console/dist/assets/app.ef5359d41bd46181.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.ff2f5b1883b6f387.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "ff2f5b1883b6f3871857186830b95f05c8901677ad2a7e1299b0b22fb40b105b",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.ff2f5b1883b6f387.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
