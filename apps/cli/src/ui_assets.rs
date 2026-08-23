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
    sha256: "df4ea9f144cc39092b6cb9a5a811c14df80ebacf6795a2c05c6dc2b86c6df67c",
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
        path: "/assets/app.1b4db69fbbe099b4.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "1b4db69fbbe099b4b314e1978e8c497edaf051b9e5cc42eb138d770235af903f",
        bytes: include_bytes!("../../../web/console/dist/assets/app.1b4db69fbbe099b4.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.63e7719ecd39fb70.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "63e7719ecd39fb708da4f598cf73821df0072630d8eaada2e691fc7dbf0633eb",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.63e7719ecd39fb70.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
