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
    sha256: "cc7060526198ab6a66602211c85a4a9d5c5ac8ccb1f9fc75b1df9fd0c3ed11f4",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.95428eb172738a1e.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "95428eb172738a1e2fee43658b761dda0afe51bc6099784165b8f56ca31a898d",
        bytes: include_bytes!("../../../web/console/dist/assets/app.95428eb172738a1e.js"),
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
        path: "/assets/bootstrap.bfc52d7a3745a1b6.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "bfc52d7a3745a1b68c2bcb60fbc656b5c096df2eb757bd3908f8706bce4369ad",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.bfc52d7a3745a1b6.js"),
        immutable: true,
    },
];

pub fn by_path(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest as _, Sha256};

    #[test]
    fn embedded_assets_exactly_match_the_checked_manifest_and_bytes() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../../../web/console/dist/asset-manifest.json"))
                .unwrap();
        let expected = manifest["assets"].as_array().unwrap();
        assert_eq!(expected.len(), ASSETS.len() + 1);
        let mut paths = std::collections::BTreeSet::new();
        for asset in std::iter::once(&INDEX).chain(ASSETS) {
            assert!(paths.insert(asset.path));
            let entry = expected.iter().find(|entry| entry["path"] == asset.path).unwrap();
            assert_eq!(entry["mime"], asset.mime);
            assert_eq!(entry["sha256"], asset.sha256);
            assert_eq!(entry["bytes"], asset.bytes.len());
            assert_eq!(format!("{:x}", Sha256::digest(asset.bytes)), asset.sha256);
        }
    }
}
