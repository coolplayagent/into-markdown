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
    sha256: "fe52025af8c1cdb636f803b75f6c5ab121683f3fe3562d9fe4263b6b76f80159",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.cabb7086262f69e2.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "cabb7086262f69e25fdb17f0c8b393eb3c506c5d975f2d11f3a1b46b7c7b0149",
        bytes: include_bytes!("../../../web/console/dist/assets/app.cabb7086262f69e2.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.202df51654199909.css",
        mime: "text/css; charset=utf-8",
        sha256: "202df51654199909ad6ad5726922d17280e6d444c130a6b54d6d594360366c02",
        bytes: include_bytes!("../../../web/console/dist/assets/app.202df51654199909.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.fd077bba1b3e031a.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "fd077bba1b3e031a3501345b78cb271d4c7e6ae7641b49277691a967191116f7",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.fd077bba1b3e031a.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
