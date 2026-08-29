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
    sha256: "f2b73388bd88185bd21bbea615200342546323f252009a3238fc48036b19671b",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.62a4c012f2178828.css",
        mime: "text/css; charset=utf-8",
        sha256: "62a4c012f2178828b0dc9233ec8bfbb8f667f5175ae6129b1bff2e3f876c4d6f",
        bytes: include_bytes!("../../../web/console/dist/assets/app.62a4c012f2178828.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.a57cd73df18d7dce.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "a57cd73df18d7dceddbbc8fdce122283cd307c96b3566e974ef7f0394c997fc0",
        bytes: include_bytes!("../../../web/console/dist/assets/app.a57cd73df18d7dce.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.dffd4bfddf6c7cc6.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "dffd4bfddf6c7cc684037ca8ab50f463b33c601c7e33853254e7b4bb60f1c5ec",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.dffd4bfddf6c7cc6.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
