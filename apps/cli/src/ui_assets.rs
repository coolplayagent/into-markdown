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
    sha256: "9f4a2611b272f9b77f5d87f0cde06e7d1bb81164f3aa5ee4325d2adccc2708d0",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.b64ec648f74773d3.css",
        mime: "text/css; charset=utf-8",
        sha256: "b64ec648f74773d3d7763be22f6f6abb2ec1b98d52eff0ea680c3a5d02e78aae",
        bytes: include_bytes!("../../../web/console/dist/assets/app.b64ec648f74773d3.css"),
        immutable: true,
    },
    Asset {
        path: "/assets/app.df15bea6ba01ebe9.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "df15bea6ba01ebe92e9d795d02506a58363bcfe97aa7843ed5fbae503a36b485",
        bytes: include_bytes!("../../../web/console/dist/assets/app.df15bea6ba01ebe9.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.69e3d4cb1fe3ce0d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "69e3d4cb1fe3ce0d227b642051bf638d2dbe1db1925edbc5477126cc84dc620e",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.69e3d4cb1fe3ce0d.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}
