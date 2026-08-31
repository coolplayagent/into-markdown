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
    sha256: "dd06345142c3f8efed9a1972f24d94d1cc1677dc2cf6637f930f0cb032948c30",
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
        path: "/assets/app.1baa18d739fe7d96.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "1baa18d739fe7d96ae06332552cea92ea285c029b67fc3ec98ac55282541a9e9",
        bytes: include_bytes!("../../../web/console/dist/assets/app.1baa18d739fe7d96.js"),
        immutable: true,
    },
    Asset {
        path: "/assets/bootstrap.ca9813798770d14d.js",
        mime: "text/javascript; charset=utf-8",
        sha256: "ca9813798770d14d9dcb75d986f6d0d02828859a32ad885abee34ae90558ac05",
        bytes: include_bytes!("../../../web/console/dist/assets/bootstrap.ca9813798770d14d.js"),
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
