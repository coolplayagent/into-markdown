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
    sha256: "65303904fe78cfb6f6f81e8e03a966d94195b9708f6a5923b3b221dc9966790c",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.3bc77a6a37815833.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "3bc77a6a378158337205f789d71e3b39945497b9dfd0768d358c5e71ee2f97b4",
        bytes: include_bytes!("../../../web/console/dist/assets/app.3bc77a6a37815833.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.9c97eeb1b4e258b9.css",
        mime: "text/css; charset=utf-8",
        sha256: "9c97eeb1b4e258b9a8196ed76adb99683a3fef3084d9f49b0fa9f18dfad57908",
        bytes: include_bytes!("../../../web/console/dist/assets/app.9c97eeb1b4e258b9.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.3bdf7a4af7959a9a.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "3bdf7a4af7959a9a8a2287e3ea5103b8ec78ab2a279c0cd8b0f9def621e50522",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.3bdf7a4af7959a9a.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
