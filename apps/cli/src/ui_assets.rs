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
    sha256: "16aa87f1e6ce06f894a3a5d3d7879f8b9acb7ce336beaa0152af22559d691f4b",
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
        path: "/assets/app.7dcd991ddb8633d9.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "7dcd991ddb8633d9ce81bdd35bd9cffcdca6ed5886d39f6136f343d7b4c9c804",
        bytes: include_bytes!("../../../web/console/dist/assets/app.7dcd991ddb8633d9.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.9a0a89e015db485d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "9a0a89e015db485d6debd0e31f8c34180114f26d287b55424090151d6d312b50",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.9a0a89e015db485d.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
