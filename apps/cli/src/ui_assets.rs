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
    sha256: "8beb5b44a8172462bc4e3268799d0c6c6cc947d3fd26f4acad331f5abe078a53",
    bytes: include_bytes!("../../../web/console/dist/index.html"),
    immutable: false,
};

pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/assets/app.1e192542e73158f7.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "1e192542e73158f7d6c0e55e20df92da274313ef2e958d59fc3ed9189e1809c4",
        bytes: include_bytes!("../../../web/console/dist/assets/app.1e192542e73158f7.js"),
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
        path: "/assets/bootstrap.e80528a7ade45a1b.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "e80528a7ade45a1b1ac9429291d0286b18015717c8d93c581c2fd604ae77bdba",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.e80528a7ade45a1b.js"),
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
