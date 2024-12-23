// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! All of the protobuf types we durably persist in the Catalog.
//!
//! These exist outside of the `mz_catalog` crate because the code generated by
//! [`prost`] for the protobuf files, takes a considerable (>40s) amount of
//! time to `build` and even `check`, yet the protobuf files rarely change. So
//! moving them into a separate crate allows folks to iterate on the
//! `mz_catalog` crate without having to pay the cost of re-compiling these
//! protobuf files.

pub mod audit_log;
pub mod serialization;

/// The current version of the `Catalog`.
///
/// We will initialize new `Catalog`s with this version, and migrate existing `Catalog`s to this
/// version. Whenever the `Catalog` changes, e.g. the protobufs we serialize in the `Catalog`
/// change, we need to bump this version.
pub const CATALOG_VERSION: u64 = 73;

/// The minimum `Catalog` version number that we support migrating from.
///
/// After bumping this we can delete the old migrations.
pub const MIN_CATALOG_VERSION: u64 = 67;

macro_rules! proto_objects {
    ( $( $x:ident ),* ) => {
        paste::paste! {
            $(
                pub mod [<objects_ $x>] {
                    include!(concat!(env!("OUT_DIR"), "/objects_", stringify!($x), ".rs"));
                }
            )*
            pub mod objects {
                include!(concat!(env!("OUT_DIR"), "/objects.rs"));
            }
        }
    };
}

proto_objects!(v67, v68, v69, v70, v71, v72, v73);

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::{BufRead, BufReader};

    use crate::{CATALOG_VERSION, MIN_CATALOG_VERSION};

    // Note: Feel free to update this path if the protos move.
    const PROTO_DIRECTORY: &str = {
        if mz_build_tools::is_bazel_build() {
            "src/catalog/protos"
        } else {
            "protos"
        }
    };

    #[mz_ore::test]
    fn test_assert_snapshots_exist() {
        // Get all of the files in the snapshot directory, with the `.proto` extension.
        let mut filenames: BTreeSet<_> = fs::read_dir(PROTO_DIRECTORY)
            .expect("failed to read protos dir")
            .map(|entry| entry.expect("failed to read dir entry").file_name())
            .map(|filename| filename.to_str().expect("utf8").to_string())
            .filter(|filename| filename.ends_with("proto"))
            .collect();

        // Assert objects.proto exists.
        assert!(filenames.remove("objects.proto"));

        // Assert snapshots exist for all of the versions we support.
        for version in MIN_CATALOG_VERSION..=CATALOG_VERSION {
            let filename = format!("objects_v{version}.proto");
            assert!(
                filenames.remove(&filename),
                "Missing snapshot for v{version}."
            );
        }

        // Common case. Check to make sure the user bumped the CATALOG_VERSION.
        if !filenames.is_empty()
            && filenames.remove(&format!("objects_v{}.proto", CATALOG_VERSION + 1))
        {
            panic!(
                "Found snapshot for v{}, please also bump `CATALOG_VERSION`.",
                CATALOG_VERSION + 1
            )
        }

        // Assert there aren't any extra snapshots.
        assert!(
            filenames.is_empty(),
            "Found snapshots for unsupported catalog versions {filenames:?}.\nIf you just increased `MIN_CATALOG_VERSION`, then please delete the old snapshots. If you created a new snapshot, please bump `CATALOG_VERSION`."
        );
    }

    #[mz_ore::test]
    fn test_assert_current_snapshot() {
        // Read the content from both files.
        let current = fs::File::open(format!("{PROTO_DIRECTORY}/objects.proto"))
            .map(BufReader::new)
            .expect("read current");
        let snapshot = fs::File::open(format!(
            "{PROTO_DIRECTORY}/objects_v{CATALOG_VERSION}.proto"
        ))
        .map(BufReader::new)
        .expect("read snapshot");

        // Read in all of the lines so we can compare the content of †he files.
        let current: Vec<_> = current
            .lines()
            .map(|r| r.expect("failed to read line from current"))
            // Filter out the package name, since we expect that to be different.
            .filter(|line| line != "package objects;")
            .collect();
        let snapshot: Vec<_> = snapshot
            .lines()
            .map(|r| r.expect("failed to read line from current"))
            // Filter out the package name, since we expect that to be different.
            .filter(|line| line != &format!("package objects_v{CATALOG_VERSION};"))
            .collect();

        // Note: objects.proto and objects_v<CATALOG_VERSION>.proto should be exactly the same. The
        // reason being, when bumping the catalog to the next version, CATALOG_VERSION + 1, we need a
        // snapshot to migrate _from_, which should be a snapshot of how the protos are today.
        // Hence why the two files should be exactly the same.
        similar_asserts::assert_eq!(current, snapshot);
    }
}