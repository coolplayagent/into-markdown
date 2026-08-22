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
    sha256: "6634100999ac6efc670a78ae847106f866c5784f7e81fb65944f550ad0ede7de",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.fbde3d09b91d7aaf.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "fbde3d09b91d7aaf0bf03666f4bf0ced0d58fbbcb9c7cb69de8fcdc36d436177",
        bytes: include_bytes!("../../../web/console/dist/assets/app.fbde3d09b91d7aaf.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.3cab554ca6063b22.css",
        mime: "text/css; charset=utf-8",
        sha256: "3cab554ca6063b22b8bf162af97b1deeec8af67e2a88deb7009b971f4306600f",
        bytes: include_bytes!("../../../web/console/dist/assets/app.3cab554ca6063b22.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.fa1ed3a3c76662a2.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "fa1ed3a3c76662a25d6e1526811325a9b50dcb03ee66546e85d651636eb8d62f",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.fa1ed3a3c76662a2.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
